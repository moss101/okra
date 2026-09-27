//! Row-48 domain tests: user/slash commands (ZCode `services/commands`).
//! The load-bearing contracts: discovery order (ALL workspace sources
//! before ALL user sources; `.zcode` strong-priority per scope), name
//! dedupe first-wins, enable overrides keyed by file path (rename
//! migrates, delete clears, write clears, empty key removed), and plugin
//! command roots that fail closed on path escapes.

use std::fs;
use std::path::{Path, PathBuf};

use okra_host::commands::{CommandsService, CommandScope, CommandSource};

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("okra-commands-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_cmd(root: &Path, rel: &str, content: &str) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, content).unwrap();
    path
}

fn cmd_md(prompt: &str) -> String {
    format!("---\ndescription: test cmd\n---\n\n{prompt}\n")
}

#[test]
fn discovery_order_workspace_before_user_and_zcode_strong_priority() {
    let home = tempdir("order-home");
    let ws = tempdir("order-ws");
    let svc = CommandsService::new(&home);

    // user ~/.agents (weak source) + workspace .agents (weak source)
    write_cmd(&home.join(".agents/commands"), "user-agents.md", &cmd_md("user agents"));
    write_cmd(&ws.join(".agents/commands"), "ws-agents.md", &cmd_md("ws agents"));
    let list = svc.list(Some(&ws)).unwrap();
    let names: Vec<&str> = list.user_commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["/ws-agents", "/user-agents"], "ALL workspace sources merge before ALL user sources, even weak ones");
    assert_eq!(list.user_commands[0].scope, CommandScope::Project);
    assert_eq!(list.user_commands[1].scope, CommandScope::Global);
    assert_eq!(list.user_commands[1].directory_source, "agents");

    // a strong ~/.zcode/commands does NOT suppress the workspace .agents
    // source — only its own scope — but DOES suppress user .agents
    write_cmd(&home.join(".zcode/commands"), "user-zcode.md", &cmd_md("user zcode"));
    let list = svc.list(Some(&ws)).unwrap();
    let names: Vec<&str> = list.user_commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["/ws-agents", "/user-zcode"],
        "workspace .agents still participates; user .agents is suppressed by user .zcode"
    );

    // within the workspace scope, .zcode suppresses .agents
    write_cmd(&ws.join(".zcode/commands"), "ws-zcode.md", &cmd_md("ws zcode"));
    let list = svc.list(Some(&ws)).unwrap();
    let names: Vec<&str> = list.user_commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["/ws-zcode", "/user-zcode"], "workspace .agents is gone: .zcode won the workspace scope");

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn same_name_dedupes_first_discovery_wins() {
    let home = tempdir("dedupe-home");
    let ws = tempdir("dedupe-ws");
    write_cmd(&home.join(".zcode/commands"), "shared.md", &cmd_md("from user"));
    write_cmd(&ws.join(".zcode/commands"), "shared.md", &cmd_md("from workspace"));
    let list = CommandsService::new(&home).list(Some(&ws)).unwrap();
    assert_eq!(list.user_commands.len(), 1);
    assert_eq!(list.user_commands[0].prompt, "from workspace", "project scope merges first");
    assert_eq!(list.user_commands[0].scope, CommandScope::Project);
    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn namespaced_names_from_nested_paths() {
    let home = tempdir("ns-home");
    write_cmd(&home.join(".zcode/commands"), "deploy/staging.md", &cmd_md("deploy to staging"));
    let list = CommandsService::new(&home).list(None).unwrap();
    assert_eq!(list.user_commands.len(), 1);
    assert_eq!(list.user_commands[0].name, "/deploy/staging");
    assert_eq!(list.user_commands[0].id, "zcodeAgent:zcode:global:/deploy/staging");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn write_update_delete_and_override_lifecycle() {
    let home = tempdir("life-home");
    let ws = tempdir("life-ws");
    let svc = CommandsService::new(&home);
    let config_path = home.join(".zcode/cli/config.json");

    // write (global), duplicate refused
    let created = svc
        .write_command_file(&okra_host::commands::WriteCommandParams {
            name: "/review".into(),
            prompt: "review the diff".into(),
            description: Some("code review".into()),
            argument_hint: Some("pr?".into()),
            project_level: false,
            workspace: None,
        })
        .unwrap();
    assert_eq!(created.name, "/review");
    assert!(created.enabled);
    let second = svc.write_command_file(&okra_host::commands::WriteCommandParams {
        name: "/review".into(),
        prompt: "x".into(),
        description: None,
        argument_hint: None,
        project_level: false,
        workspace: None,
    });
    assert!(matches!(second, Err(okra_host::commands::CommandsError::AlreadyExists(_))));

    // project write without a workspace is a contract error
    let missing = svc.write_command_file(&okra_host::commands::WriteCommandParams {
        name: "/x".into(),
        prompt: "x".into(),
        description: None,
        argument_hint: None,
        project_level: true,
        workspace: None,
    });
    assert!(matches!(missing, Err(okra_host::commands::CommandsError::MissingWorkspace)));

    // disable → list reflects it (config override keyed by file path)
    svc.set_command_enabled(&created.file_path, false).unwrap();
    let list = svc.list(None).unwrap();
    assert!(!list.user_commands.iter().find(|c| c.name == "/review").unwrap().enabled);
    let config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert_eq!(
        config["command"][created.file_path.to_string_lossy().as_ref()]["enable"],
        serde_json::json!(false)
    );

    // rename MIGRATES the override — the disabled command must not resurrect
    let renamed = svc
        .update_command_file(
            &okra_host::commands::WriteCommandParams {
                name: "/review2".into(),
                prompt: "review the diff harder".into(),
                description: created.description.clone(),
                argument_hint: created.argument_hint.clone(),
                project_level: false,
                workspace: None,
            },
            Some(&created.file_path),
        )
        .unwrap();
    assert_eq!(renamed.name, "/review2");
    assert!(!renamed.enabled, "renamed command keeps its disabled override");
    assert!(!created.file_path.exists(), "old file removed on rename");
    let list = svc.list(None).unwrap();
    assert_eq!(list.user_commands.len(), 1, "old name gone from discovery");
    assert_eq!(list.user_commands[0].name, "/review2");

    // delete clears the override; the empty `command` key is removed
    svc.delete_command_file(&renamed.file_path).unwrap();
    let list = svc.list(None).unwrap();
    assert!(list.user_commands.is_empty());
    let config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(config.get("command").is_none(), "empty command config key removed: {config}");

    // deleting a missing file is idempotent success
    let gone = svc.delete_command_file(&renamed.file_path);
    assert!(gone.is_ok());

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn plugin_commands_fail_closed_on_path_escapes_and_honor_enable_lattice() {
    let home = tempdir("plugin-home");
    let plugin = tempdir("plugin-root");

    // manifest with a traversal command path plus a good nested one
    fs::create_dir_all(plugin.join(".zcode-plugin")).unwrap();
    fs::write(
        plugin.join(".zcode-plugin/plugin.json"),
        r#"{ "name": "team-tools", "commands": ["./commands", "./../escape", "/abs/path"] }"#,
    )
    .unwrap();
    write_cmd(&plugin.join("commands"), "deploy.md", &cmd_md("plugin deploy"));
    write_cmd(&plugin.join("commands/nested"), "inner.md", &cmd_md("plugin inner"));

    let svc = CommandsService::new(&home);
    let config_path = home.join(".zcode/cli/config.json");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(
        &config_path,
        serde_json::json!({
            "plugins": { "dirs": [plugin.to_string_lossy()] }
        })
        .to_string(),
    )
    .unwrap();

    let plugins = svc.plugin_commands().unwrap();
    let names: Vec<&str> = plugins.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["/deploy", "/nested/inner"], "traversal and absolute roots rejected; good root scanned recursively");
    assert_eq!(plugins[0].source, CommandSource::Plugin);
    assert_eq!(plugins[0].plugin_name.as_deref(), Some("team-tools"));
    assert_eq!(plugins[0].plugin_marketplace.as_deref(), Some("inline"));
    assert!(plugins[0].plugin_enabled.unwrap());

    // the plugin list also flows through list()
    let list = svc.list(None).unwrap();
    assert_eq!(list.plugin_commands.len(), 2);
    assert!(list.user_commands.is_empty());

    // per-file override composes with plugin enable
    svc.set_command_enabled(&plugins[0].file_path, false).unwrap();
    let plugins = svc.plugin_commands().unwrap();
    assert!(!plugins.iter().find(|c| c.name == "/deploy").unwrap().enabled);
    assert!(plugins.iter().find(|c| c.name == "/nested/inner").unwrap().enabled);

    // plugin disabled entirely → no commands
    fs::write(
        &config_path,
        serde_json::json!({
            "plugins": { "dirs": [plugin.to_string_lossy()], "enabledPlugins": { "team-tools@inline": false } }
        })
        .to_string(),
    )
    .unwrap();
    assert!(svc.plugin_commands().unwrap().is_empty());

    // suppressed builtin official cache entries are skipped
    fs::write(
        &config_path,
        serde_json::json!({
            "plugins": {
                "suppressedBuiltins": ["team-tools@zcode-plugins-official"]
            }
        })
        .to_string(),
    )
    .unwrap();
    // stage the plugin into the official cache layout: cache/<marketplace>/<name>/<version>
    let cache = home.join("storage/cli/plugins/cache/zcode-plugins-official/team-tools/1.0.0");
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    let _ = fs::remove_dir_all(&cache);
    let _ = fs::create_dir_all(&cache);
    let copy = |rel: &str| {
        fs::create_dir_all(cache.join(rel).parent().unwrap()).unwrap();
        let _ = fs::copy(plugin.join(rel), cache.join(rel));
    };
    copy(".zcode-plugin/plugin.json");
    copy("commands/deploy.md");
    let plugins = svc.plugin_commands().unwrap();
    assert!(
        plugins.iter().all(|c| c.plugin_name.as_deref() != Some("team-tools")),
        "suppressed builtin contributes nothing from the cache: {plugins:?}"
    );

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&plugin);
}

#[test]
fn plugin_default_commands_dir_only_when_manifest_omits_the_key() {
    let home = tempdir("plugin-default-home");

    // manifest WITHOUT a commands key → default commands/ dir applies
    let plugin_a = tempdir("plugin-default-a");
    fs::create_dir_all(plugin_a.join(".zcode-plugin")).unwrap();
    fs::write(plugin_a.join(".zcode-plugin/plugin.json"), r#"{ "name": "default-root" }"#).unwrap();
    write_cmd(&plugin_a.join("commands"), "alpha.md", &cmd_md("alpha"));

    // manifest WITH a commands key that resolves nowhere → NO default fallback
    let plugin_b = tempdir("plugin-default-b");
    fs::create_dir_all(plugin_b.join(".zcode-plugin")).unwrap();
    fs::write(
        plugin_b.join(".zcode-plugin/plugin.json"),
        r#"{ "name": "empty-root", "commands": "./does-not-exist" }"#,
    )
    .unwrap();
    write_cmd(&plugin_b.join("commands"), "beta.md", &cmd_md("beta"));

    let svc = CommandsService::new(&home);
    let config_path = home.join(".zcode/cli/config.json");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(
        &config_path,
        serde_json::json!({ "plugins": { "dirs": [plugin_a.to_string_lossy(), plugin_b.to_string_lossy()] } })
            .to_string(),
    )
    .unwrap();

    let plugins = svc.plugin_commands().unwrap();
    let names: Vec<&str> = plugins.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["/alpha"], "key present but unresolvable → no default fallback");

    let _ = fs::remove_dir_all(&home);
    let _ = fs::remove_dir_all(&plugin_a);
    let _ = fs::remove_dir_all(&plugin_b);
}
