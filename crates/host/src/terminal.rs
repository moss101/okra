//! Terminal/PTY domain — the M3 strangler's second host domain
//! (MASTER-PLAN §3 #48: "terminal/PTY" among the re-homed ZCode domains).
//!
//! Real pseudo-terminal sessions via `portable-pty`: programs see a TTY
//! (`isatty` true), get a controllable size, and stream output through the
//! host. Managed by id like every host domain (terminal sessions are the
//! long-lived resources the workbench UI attaches to).

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalSize {
    pub rows: u16,
    pub cols: u16,
}

impl Default for TerminalSize {
    fn default() -> Self {
        TerminalSize { rows: 24, cols: 80 }
    }
}

pub struct TerminalSession {
    writer: Box<dyn Write + Send>,
    reader: Box<dyn Read + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

impl TerminalSession {
    /// Spawn `program` attached to a fresh PTY in `cwd`. The reader is
    /// split out (returned alongside) so an output-pump thread can own it
    /// while the session keeps writer/resize — a blocking read never holds
    /// the session lock.
    pub fn spawn_split(
        program: &str,
        args: &[String],
        cwd: &std::path::Path,
        size: TerminalSize,
    ) -> Result<(TerminalSession, Box<dyn Read + Send>), String> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("openpty: {e}"))?;
        let mut cmd = CommandBuilder::new(program);
        cmd.args(args);
        cmd.cwd(cwd);
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| format!("pty spawn: {e}"))?;
        // drop our handle to the slave so EOF propagates to the reader
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("pty reader: {e}"))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| format!("pty writer: {e}"))?;
        let session = TerminalSession {
            writer,
            reader: Box::new(std::io::empty()),
            master: pair.master,
            child: Mutex::new(child),
        };
        Ok((session, Box::new(reader)))
    }

    /// Spawn `program` attached to a fresh PTY in `cwd` (reader kept on
    /// the session — `read`/`read_to_end` style callers).
    pub fn spawn(
        program: &str,
        args: &[String],
        cwd: &std::path::Path,
        size: TerminalSize,
    ) -> Result<TerminalSession, String> {
        let (mut session, reader) = Self::spawn_split(program, args, cwd, size)?;
        // original contract: the session owns its reader (read/read_to_end)
        session.reader = reader;
        Ok(session)
    }

    /// Write to the terminal's input (as if typed).
    pub fn write(&mut self, data: &[u8]) -> Result<usize, String> {
        self.writer.write(data).map_err(|e| format!("pty write: {e}"))
    }

    /// Read available output from the terminal (blocking up to the reader's
    /// semantics; callers drive timeouts).
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        self.reader.read(buf).map_err(|e| format!("pty read: {e}"))
    }

    /// Read until EOF (child exited and closed the pty), collecting output.
    pub fn read_to_end(&mut self) -> Result<String, String> {
        let mut out = String::new();
        self.reader
            .read_to_string(&mut out)
            .map_err(|e| format!("pty read: {e}"))?;
        Ok(out)
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), String> {
        self.master
            .resize(PtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| format!("pty resize: {e}"))
    }

    /// Wait for the child to exit; returns its exit code.
    pub fn wait(&self) -> Result<i32, String> {
        let mut child = self.child.lock().unwrap();
        child
            .wait()
            .map_err(|e| format!("pty wait: {e}"))
            .map(|s| s.exit_code() as i32)
    }
}

/// The host domain: manages named terminal sessions.
#[derive(Default)]
pub struct TerminalHost {
    sessions: std::collections::HashMap<String, TerminalSession>,
}

impl TerminalHost {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn open(
        &mut self,
        id: &str,
        program: &str,
        args: &[String],
        cwd: &std::path::Path,
    ) -> Result<TerminalSize, String> {
        let session = TerminalSession::spawn(program, args, cwd, TerminalSize::default())?;
        self.sessions.insert(id.to_string(), session);
        Ok(TerminalSize::default())
    }

    pub fn get(&mut self, id: &str) -> Option<&mut TerminalSession> {
        self.sessions.get_mut(id)
    }

    pub fn close(&mut self, id: &str) -> bool {
        self.sessions.remove(id).is_some()
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.sessions.keys().cloned().collect();
        ids.sort();
        ids
    }
}
