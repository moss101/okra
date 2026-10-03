//! Windows restricted-token child spawn (the g5 unlock, scoped in
//! docs/m6-windows-port.md). Phase 1 — the cross-platform pieces:
//!
//! - [`build_env_block`]: the `CreateProcessAsUserW` environment block
//!   (wide, `key=value`, sorted, double-NUL terminated) — pure logic,
//!   unit-tested on every OS;
//! - [`quote_cmdline`]: argv re-quoted into the single mutable wide
//!   command line `CreateProcessAsUserW` parses (std::process::Command
//!   cannot drive it — the token API needs the raw call).
//!
//! Phase 2 (unsafe, windows-only, next): `OpenProcessToken` →
//! `CreateRestrictedToken(DISABLE_MAX_PRIVILEGE)` → `CreateProcessAsUserW`
//! with pipe inheritance via STARTUPINFOW. Verified signatures are in
//! docs/m6-windows-port.md. Nothing here executes on unix.

/// Build a `CreateProcessAsUserW` environment block from sorted pairs:
/// each `key=value` as UTF-16 with a NUL, the whole block ending in a
/// second NUL. Keys are sorted (Windows treats env blocks as sorted;
/// `GetEnvironmentStrings` returns them sorted and some children rely on
/// it). A key without `=` can never appear (callers pass pairs).
pub fn build_env_block(pairs: &[(String, String)]) -> Vec<u16> {
    let mut sorted: Vec<&(String, String)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in sorted {
        for unit in format!("{k}={v}").encode_utf16() {
            block.push(unit);
        }
        block.push(0);
    }
    block.push(0);
    block
}

/// Quote one argv element for a Windows command line: wrap in quotes when
/// the element contains a space, quote, or tab; double any embedded
/// quotes (the CommandLineToArgvW convention `CreateProcessAsUserW`
/// parses).
pub fn quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| c == ' ' || c == '\t' || c == '"' || c == '\n')
    {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        if c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// argv → one command line (space-joined, quoted per [`quote_arg`]).
pub fn build_command_line(argv: &[String]) -> String {
    argv.iter().map(|a| quote_arg(a)).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_block_is_sorted_utf16_double_nul() {
        let block = build_env_block(&[
            ("ZLAST".into(), "1".into()),
            ("AFIRST".into(), "one".into()),
        ]);
        let as_string: String = String::from_utf16(&block).unwrap();
        // sorted: AFIRST first
        assert!(as_string.starts_with("AFIRST=one\u{0}ZLAST=1\u{0}\u{0}"));
    }

    #[test]
    fn empty_pairs_produce_the_terminator_only() {
        // an empty list is a single NUL (the list terminator); a non-empty
        // list ends with its own entry NUL + the terminator
        assert_eq!(build_env_block(&[]), vec![0]);
        let one = build_env_block(&[("K".into(), "v".into())]);
        assert_eq!(one, vec!['K' as u16, '=' as u16, 'v' as u16, 0, 0]);
    }

    #[test]
    fn quoting_wraps_spaces_and_escapes_quotes() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("has space"), "\"has space\"");
        assert_eq!(quote_arg("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn command_line_joins_quoted_elements() {
        let line = build_command_line(&[
            "C:\\bin\\okra.exe".into(),
            "run-subagent".into(),
            "--task".into(),
            "two words".into(),
        ]);
        assert_eq!(
            line,
            "C:\\bin\\okra.exe run-subagent --task \"two words\""
        );
    }
}

/// Spawn the confined child under a restricted token (every privilege
/// dropped via DISABLE_MAX_PRIVILEGE) and capture its output — the
/// windows counterpart of `std::process::Command::output()` for the g5
/// confined launches (docs/m6-windows-port.md, restricted-token scope).
///
/// Fail-closed: any API error before creation aborts the launch; the
/// child either starts as the restricted token or the call errors.
#[cfg(windows)]
pub fn spawn_restricted_output(
    app: &str,
    argv: &[String],
    extra_env: &[(String, String)],
) -> Result<std::process::Output, String> {
    use std::collections::BTreeMap;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, SECURITY_ATTRIBUTES, TOKEN_DUPLICATE,
        TOKEN_QUERY,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_APPEND_DATA, FILE_WRITE_DATA};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcessToken,
        WaitForSingleObject, STARTF_USESTDHANDLES, STARTUPINFOW, PROCESS_INFORMATION,
    };
    use windows_sys::Win32::Foundation::GENERIC_WRITE;

    let err = |step: &str| -> String {
        format!("restricted spawn failed at {step}: {}", unsafe { GetLastError() })
    };

    // 1. the daemon's own token, duplicated into a restricted form: every
    //    privilege dropped, nothing else changed (the child keeps its
    //    group memberships and user identity — it is US, minus power)
    let mut tok: HANDLE = std::ptr::null_mut();
    let rc = unsafe {
        OpenProcessToken(GetCurrentProcess(), (TOKEN_DUPLICATE | TOKEN_QUERY) as u32, &mut tok)
    };
    if rc == 0 {
        return Err(err("OpenProcessToken"));
    }
    let mut restricted: HANDLE = std::ptr::null_mut();
    let rc = unsafe {
        CreateRestrictedToken(
            tok,
            DISABLE_MAX_PRIVILEGE,
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            &mut restricted,
        )
    };
    unsafe { CloseHandle(tok) };
    if rc == 0 {
        return Err(err("CreateRestrictedToken"));
    }

    // 2. stdout/stderr pipes; the write ends inherit into the child
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut out_read: HANDLE = std::ptr::null_mut();
    let mut out_write: HANDLE = std::ptr::null_mut();
    let mut err_read: HANDLE = std::ptr::null_mut();
    let mut err_write: HANDLE = std::ptr::null_mut();
    if unsafe { CreatePipe(&mut out_read, &mut out_write, &sa, 0) } == 0 {
        unsafe { CloseHandle(restricted) };
        return Err(err("CreatePipe(stdout)"));
    }
    if unsafe { CreatePipe(&mut err_read, &mut err_write, &sa, 0) } == 0 {
        unsafe { CloseHandle(restricted); CloseHandle(out_read); CloseHandle(out_write) };
        return Err(err("CreatePipe(stderr)"));
    }
    // the READ ends must NOT leak into the child (the child holds the
    // write ends only — or reads never reach EOF)
    unsafe { SetHandleInformation(out_read, HANDLE_FLAG_INHERIT, 0) };
    unsafe { SetHandleInformation(err_read, HANDLE_FLAG_INHERIT, 0) };

    // 3. the command line + environment block + startup info
    let mut argv_full: Vec<String> = Vec::with_capacity(argv.len() + 1);
    argv_full.push(app.to_string());
    argv_full.extend_from_slice(argv);
    let mut cmd_line: Vec<u16> =
        build_command_line(&argv_full).encode_utf16().chain(std::iter::once(0)).collect();
    let mut env_pairs: Vec<(String, String)> = std::env::vars().collect();
    env_pairs.extend_from_slice(extra_env);
    let env_block = build_env_block(&env_pairs);
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = std::ptr::null_mut();
    si.hStdOutput = out_write;
    si.hStdError = err_write;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // 4. create AS the restricted token; NULL cwd inherits the daemon's
    let app_wide: Vec<u16> = app.encode_utf16().chain(std::iter::once(0)).collect();
    let created = unsafe {
        CreateProcessAsUserW(
            restricted,
            app_wide.as_ptr(),
            cmd_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1, // inherit the pipe write handles
            0,
            env_block.as_ptr(),
            std::ptr::null(),
            &mut si,
            &mut pi,
        )
    };
    // the parent's copies of the write ends die here — the child holds
    // the only writer handles, so our reads see EOF when it exits
    unsafe { CloseHandle(out_write); CloseHandle(err_write) };
    if created == 0 {
        let e = err("CreateProcessAsUserW");
        unsafe { CloseHandle(restricted); CloseHandle(out_read); CloseHandle(err_read) };
        return Err(e);
    }
    unsafe { CloseHandle(restricted) };

    // 5. drain stderr on a helper thread (a full stderr pipe would
    //    deadlock a stdout-only drain), wait, then read stdout to EOF
    let err_handle = err_read;
    let err_drain = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let mut n: u32 = 0;
            if unsafe { windows_sys::Win32::Storage::FileSystem::ReadFile(err_handle, chunk.as_mut_ptr(), chunk.len() as u32, &mut n, std::ptr::null_mut()) } == 0 && n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
        }
        buf
    });
    let mut stdout = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let mut n: u32 = 0;
        if unsafe { windows_sys::Win32::Storage::FileSystem::ReadFile(out_read, chunk.as_mut_ptr(), chunk.len() as u32, &mut n, std::ptr::null_mut()) } == 0 && n == 0 {
            break;
        }
        stdout.extend_from_slice(&chunk[..n as usize]);
    }
    let stderr = err_drain.join().unwrap_or_default();
    unsafe { CloseHandle(out_read); CloseHandle(err_read) };

    // 6. the exit code
    let mut code: u32 = 0;
    unsafe { GetExitCodeProcess(pi.hProcess, &mut code) };
    unsafe { CloseHandle(pi.hProcess); CloseHandle(pi.hThread) };

    Ok(std::process::Output {
        status: std::os::windows::process::ExitStatusExt::from_raw(code as i32 as u32),
        stdout,
        stderr,
    })
}
