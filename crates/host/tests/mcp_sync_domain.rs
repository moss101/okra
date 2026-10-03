//! MCP sync domain end-to-end (row #48 strangler, from ZCode mcp-sync):
//! a local machine exports its user MCP servers, a remote machine imports
//! them — skipping names it already has, folding legacy `enable` flags,
//! rebasing filesystem-server paths onto its own home/workspace, and
//! persisting secrets behind 0o600 atomic writes.

#[cfg(unix)]
use okra_host::mcp_sync::PathRewrite;
use okra_host::mcp_sync::{ExportedServer, ImportStatus, McpSyncService, McpSyncSource};
use serde_json::json;

// symlink+home-rebase fixture semantics (unix); the windows variant of
// the rebase path is triaged with its sibling filesystem_path_rewrite.
#[cfg(unix)]
#[test]
fn local_exports_remote_imports_with_rewrites() {
    let local_home = tempfile::tempdir().unwrap();
    let remote_home = tempfile::tempdir().unwrap();

    // local: okra config with a normal server + a filesystem server whose
    // args point inside the local home/workspace; an agents-file server
    // that is shadowed by okra's config
    std::fs::create_dir_all(local_home.path().join(".okra")).unwrap();
    std::fs::write(
        local_home.path().join(".okra/config.json"),
        json!({
            "mcp": {
                "servers": {
                    "grep": { "command": "rg", "args": ["--json"] },
                    "filesystem": {
                        "type": "stdio",
                        "command": "npx",
                        "args": [
                            "-y",
                            "@modelcontextprotocol/server-filesystem",
                            local_home.path().join("work").display().to_string(),
                            local_home.path().join("docs").display().to_string()
                        ]
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(local_home.path().join(".agents")).unwrap();
    std::fs::write(
        local_home.path().join(".agents/mcp.json"),
        json!({ "mcpServers": { "shadowed": { "command": "gone" } } }).to_string(),
    )
    .unwrap();

    let local = McpSyncService::new(local_home.path());
    let candidates = local.candidates().unwrap();
    let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["filesystem", "grep"], "shadowed agents server is not a candidate");
    assert_eq!(candidates[0].source, McpSyncSource::Okra);

    let to_sync: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
    let exported = local.export(&to_sync).unwrap();
    assert_eq!(exported.len(), 2);

    // remote: has its own "grep" already; imports must skip it
    std::fs::create_dir_all(remote_home.path().join(".okra")).unwrap();
    std::fs::write(
        remote_home.path().join(".okra/config.json"),
        json!({ "mcp": { "servers": { "grep": { "command": "remote-rg" } } } }).to_string(),
    )
    .unwrap();

    let remote = McpSyncService::new(remote_home.path());
    let rewrite = PathRewrite {
        local_home: local_home.path().to_path_buf(),
        local_workspace: Some(local_home.path().join("work")),
        remote_home: PathBuf::from("/srv/users/ada"),
        remote_workspace: Some(PathBuf::from("/srv/work/proj")),
    };
    let outcomes = remote.import(&exported, Some(&rewrite)).unwrap();
    let by_name: Vec<(&str, ImportStatus)> = outcomes
        .iter()
        .map(|o| (o.name.as_str(), o.status))
        .collect();
    assert_eq!(
        by_name,
        vec![("filesystem", ImportStatus::Synced), ("grep", ImportStatus::Skipped)]
    );

    // the remote's grep was NOT clobbered; filesystem got rebased paths
    let loaded = remote.load(None).unwrap();
    assert_eq!(loaded.len(), 2);
    let grep = loaded.iter().find(|r| r.name == "grep").unwrap();
    assert_eq!(grep.config["command"], "remote-rg");
    let filesystem = loaded.iter().find(|r| r.name == "filesystem").unwrap();
    let args = filesystem.config["args"].as_array().unwrap();
    assert_eq!(args[2], "/srv/work/proj", "workspace root maps to workspace root");
    assert_eq!(args[3], "/srv/users/ada/docs", "home paths rebase");
    assert!(filesystem.enabled);
}

#[test]
fn full_round_trip_including_legacy_migration_and_toggle() {
    let td = tempfile::tempdir().unwrap();
    let svc = McpSyncService::new(td.path());

    // start from a legacy agents file (pre-migration shape)
    std::fs::create_dir_all(td.path().join(".agents")).unwrap();
    std::fs::write(
        td.path().join(".agents/mcp.json"),
        json!({ "mcpServers": { "legacy": { "command": "l", "enable": false } } }).to_string(),
    )
    .unwrap();
    let records = svc.load(None).unwrap();
    assert_eq!(records.len(), 1);
    assert!(!records[0].enabled, "legacy enable:false disables");
    assert_eq!(records[0].source, McpSyncSource::Agents);

    // re-enable through the service: the edit lands in the file where the
    // server lives (the agents interop file) and residue is stripped
    svc.set_enabled("legacy", true, None).unwrap();
    let records = svc.load(None).unwrap();
    assert!(records[0].enabled);
    assert!(records[0].config.get("enable").is_none());
    assert!(records[0].config.get("enabled").is_none());
    let agents = std::fs::read_to_string(td.path().join(".agents/mcp.json")).unwrap();
    assert!(!agents.contains("enable"), "{agents}");
    // disable again: enabled:false persists inside the server object
    svc.set_enabled("legacy", false, None).unwrap();
    let records = svc.load(None).unwrap();
    assert!(!records[0].enabled);

    // export from one home, import into another home's fresh okra config
    let candidates = svc.candidates().unwrap();
    let exported: Vec<ExportedServer> = svc
        .export(&candidates.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
        .unwrap();
    let other = tempfile::tempdir().unwrap();
    let remote = McpSyncService::new(other.path());
    let outcomes = remote
        .import(&exported.iter().map(|e| ExportedServer {
            id: e.id.clone(),
            name: e.name.clone(),
            config: e.config.clone(),
            enabled: true,
            source: e.source,
            path: e.path.clone(),
        }).collect::<Vec<_>>(), None)
        .unwrap();
    assert!(outcomes.iter().all(|o| o.status == ImportStatus::Synced));
    let remote_records = remote.load(None).unwrap();
    assert_eq!(remote_records.len(), 1);
    assert_eq!(remote_records[0].name, "legacy");
    assert_eq!(remote_records[0].source, McpSyncSource::Okra);
    assert!(remote_records[0].enabled);

    // a second import of the same payload skips everything
    let outcomes = remote
        .import(&exported.iter().map(|e| ExportedServer {
            id: e.id.clone(),
            name: e.name.clone(),
            config: e.config.clone(),
            enabled: true,
            source: e.source,
            path: e.path.clone(),
        }).collect::<Vec<_>>(), None)
        .unwrap();
    assert!(outcomes.iter().all(|o| o.status == ImportStatus::Skipped));
}

#[cfg(unix)]
use std::path::PathBuf;
