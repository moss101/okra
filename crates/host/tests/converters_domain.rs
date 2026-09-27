//! #46 converter remainder: Claude Code plugin + Gemini CLI extension →
//! okra's data-only PluginManifest. The converters move DATA only, fail
//! closed on path escapes, drop foreign shapes with honest diagnostics,
//! and their output must pass okra's own manifest gate (reparsed()).

// Test harness note: exercises converter code over real tempdir layouts.
#![allow(clippy::disallowed_methods)]

use std::fs;
use std::path::PathBuf;

use okra_host::plugins::{
    convert_claude_plugin, convert_gemini_extension, DiagnosticSeverity,
};

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("okra-convert-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &std::path::Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn claude_plugin_converts_to_data_only_manifest() {
    let root = tempdir("claude");
    write(
        &root,
        ".claude-plugin/plugin.json",
        r#"{
            "name": "team-pack",
            "version": "1.2.0",
            "description": "team tooling",
            "keywords": ["deploy", "review"],
            "homepage": "https://example.com",
            "license": "MIT",
            "author": { "name": "Ada", "email": "ada@example.com", "url": "https://ada.example.com" },
            "commands": "commands",
            "agents": ["./agents"],
            "skills": "skills",
            "mcpServers": "./.mcp.json",
            "hooks": { "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "rm -rf /" }] }] },
            "outputStyles": "styles"
        }"#,
    );
    write(&root, "commands", "");
    write(&root, ".mcp.json", r#"{ "mcpServers": { "docs": { "command": "docs-server" } } }"#);

    let out = convert_claude_plugin(&root).unwrap();
    assert_eq!(out.manifest.name, "team-pack");
    assert_eq!(out.manifest.version.as_deref(), Some("1.2.0"));
    assert_eq!(out.manifest.commands, vec!["./commands"]);
    assert_eq!(out.manifest.agents, vec!["./agents"]);
    assert_eq!(out.manifest.skills, vec!["./skills"]);
    assert_eq!(out.manifest.author.as_ref().unwrap().name.as_deref(), Some("Ada"));
    // url has no okra field — kept out, not invented
    let author_json = serde_json::to_value(out.manifest.author.clone()).unwrap();
    assert!(author_json.get("url").is_none(), "{author_json}");

    // .mcp.json resolved through the confined path
    let servers = out.manifest.mcp_servers.as_ref().unwrap();
    assert!(servers.contains_key("docs"), "{servers:?}");

    // foreign shapes dropped with honest diagnostics — never smuggled in
    let warnings: Vec<&str> = out
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(warnings.iter().any(|w| w.starts_with("hooks:")), "{warnings:?}");
    assert!(warnings.iter().any(|w| w.starts_with("outputStyles:")), "{warnings:?}");
    assert!(out.manifest.hooks.is_none(), "no hooks data crosses over");
    assert!(
        !serde_json::to_string(&out.manifest).unwrap().contains("rm -rf"),
        "hook command strings must not leak into the manifest"
    );

    // the converted output satisfies okra's own data-only gate
    assert!(out.reparsed().manifest.is_some(), "{:?}", out.reparsed().diagnostics);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn claude_plugin_rejects_escapes_and_bad_mcp_paths() {
    let root = tempdir("claude-escape");
    write(
        &root,
        ".claude-plugin/plugin.json",
        r#"{
            "name": "escape-pack",
            "commands": ["../outside", "/absolute"],
            "mcpServers": "./../elsewhere.json"
        }"#,
    );
    let out = convert_claude_plugin(&root).unwrap();
    assert!(out.manifest.commands.is_empty(), "escaping paths dropped");
    assert!(out.manifest.mcp_servers.is_none(), "escaping mcp path dropped");
    let errors = out
        .diagnostics
        .iter()
        .filter(|d| d.severity == DiagnosticSeverity::Error)
        .count();
    assert_eq!(errors, 3, "three dropped paths, three errors: {:?}", out.diagnostics);
    assert!(out.reparsed().manifest.is_some());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn gemini_extension_converts_toml_commands_to_markdown() {
    let root = tempdir("gemini");
    write(
        &root,
        "gemini-extension.json",
        r#"{
            "name": "gemini-tools",
            "version": "0.3.0",
            "description": "gemini pack",
            "mcpServers": { "search": { "httpUrl": "https://search.example.com" } }
        }"#,
    );
    write(&root, "commands/deploy.toml", "prompt = \"deploy the service\"\ndescription = \"deploy cmd\"\n");
    write(&root, "commands/plain.toml", "prompt = \"just run\"\n");
    write(&root, "commands/broken.toml", "this is not toml ][");

    let out = convert_gemini_extension(&root).unwrap();
    assert_eq!(out.manifest.name, "gemini-tools");
    assert_eq!(out.manifest.commands, vec!["./commands"]);
    assert!(out.manifest.mcp_servers.as_ref().unwrap().contains_key("search"));

    // TOML → markdown, description becomes frontmatter
    assert_eq!(out.generated_files.len(), 2, "broken.toml is a diagnostic, not a file: {:?}", out.generated_files);
    let (deploy_path, deploy_md) = &out.generated_files[0];
    assert_eq!(deploy_path, "./commands/deploy.md");
    assert_eq!(deploy_md, "---\ndescription: deploy cmd\n---\n\ndeploy the service\n");
    let (plain_path, plain_md) = &out.generated_files[1];
    assert_eq!(plain_path, "./commands/plain.md");
    assert_eq!(plain_md, "just run\n");

    // the broken file is an error diagnostic naming the file
    assert!(
        out.diagnostics
            .iter()
            .any(|d| d.message.starts_with("commands/broken.toml:")),
        "{:?}",
        out.diagnostics
    );
    assert!(out.reparsed().manifest.is_some());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn non_plugins_are_errors_not_empty_conversions() {
    let empty = tempdir("empty");
    assert!(convert_claude_plugin(&empty)
        .unwrap_err()
        .contains("not a Claude Code plugin"));
    assert!(convert_gemini_extension(&empty)
        .unwrap_err()
        .contains("not a Gemini CLI extension"));
    let _ = fs::remove_dir_all(&empty);
}
