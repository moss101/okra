//! Settings-sync domain end-to-end (MASTER-PLAN #48/#46 groundwork, from
//! ZCode settings-sync): a machine with several external coding agents'
//! data (Claude Code, Codex CLI, qwen code) is discovered, and selected
//! skills + commands are imported into okra's own directories — with the
//! skip-reason state machine keeping every import idempotent.

use okra_host::settings_sync::{
    DiscoveryResult, ImportMode, SettingsSyncService, SkipReason, SyncAgent, SyncImportStatus,
};
use std::path::Path;

fn write_skill(root: &Path, rel: &str, name: &str, description: &str) {
    let dir = root.join(rel);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\nversion: 1.0\ndescription: {description}\n---\n\nBody.\n"),
    )
    .unwrap();
}

fn write_command(root: &Path, rel: &str, body: &str) {
    let file = root.join(rel);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, body).unwrap();
}

#[test]
fn converts_multi_agent_home_into_okra() {
    let td = tempfile::tempdir().unwrap();
    let home = td.path();
    let ws = td.path().join("project");

    // three agents' data on one machine
    write_skill(&home.join(".claude/skills"), "code-review", "code-review", "claude skill");
    write_skill(&home.join(".codex/skills"), "ship", "ship", "codex skill");
    write_skill(&home.join(".qwen/skills"), "translate", "translate", "qwen skill");
    // same skill name as claude's, from a different agent: sameNameExists
    write_skill(&home.join(".qwen/skills"), "code-review-copy", "code-review", "duplicate name");
    write_command(
        &home.join(".claude/commands"),
        "deploy.md",
        "---\ndescription: ship it\nargument-hint: <env>\n---\nDeploy now.\n",
    );
    write_command(
        &home.join(".codex/commands"),
        "nested/audit.md",
        "plain audit body\n",
    );
    // a workspace-scoped skill only found with the project open
    write_skill(&ws.join(".claude/skills"), "local-fix", "local-fix", "project skill");

    let svc = SettingsSyncService::new(home);

    // --- discovery sees each agent separately
    let skills: DiscoveryResult = svc.discover_skills(Some(&ws));
    let claude = skills
        .agents
        .iter()
        .find(|a| a.agent == SyncAgent::ClaudeCode)
        .unwrap();
    assert_eq!(claude.discovered_count, 2, "1 global + 1 project");
    assert!(claude.selected_by_default);
    let qwen = skills
        .agents
        .iter()
        .find(|a| a.agent == SyncAgent::QwenCode)
        .unwrap();
    assert_eq!(qwen.discovered_count, 2, "qwen has 2 global skills");

    let commands = svc.discover_commands(Some(&ws));
    let claude_commands = commands
        .agents
        .iter()
        .find(|a| a.agent == SyncAgent::ClaudeCode)
        .unwrap();
    assert_eq!(claude_commands.discovered_count, 1);
    assert!(!claude_commands.selected_by_default, "commands never default-selected");

    // --- import all skills in copy mode
    let skill_candidates = svc.collect_skill_candidates(Some(&ws));
    let results = svc.import(&skill_candidates, ImportMode::Copy, "skills");
    let imported = results
        .iter()
        .filter(|r| r.status == SyncImportStatus::Imported)
        .count();
    let skipped_same_name = results
        .iter()
        .filter(|r| r.skip_reason == Some(SkipReason::SameNameExists))
        .count();
    assert_eq!(imported, 4, "5 candidates: the qwen dup of code-review skips");
    assert_eq!(skipped_same_name, 1, "the qwen duplicate of code-review");

    // okra owns the converted copies now (workspace-scoped candidates
    // land under the workspace's .okra, mirroring the donor)
    let okra_skills = home.join(".okra/skills");
    assert!(okra_skills.join("code-review/SKILL.md").exists());
    assert!(okra_skills.join("ship/SKILL.md").exists());
    assert!(okra_skills.join("translate/SKILL.md").exists());
    assert!(ws.join(".okra/skills/local-fix/SKILL.md").exists());

    // --- import the commands too
    let command_candidates = svc.collect_command_candidates(Some(&ws));
    let command_results = svc.import(&command_candidates, ImportMode::Copy, "commands");
    assert!(command_results
        .iter()
        .all(|r| r.status == SyncImportStatus::Imported));
    let okra_commands = home.join(".okra/commands");
    assert!(okra_commands.join("deploy.md").exists());
    assert!(okra_commands.join("nested/audit.md").exists());

    // --- re-import: everything skips (imported ones via targetExists,
    // the never-imported duplicate still via sameNameExists)
    let again = svc.import(&skill_candidates, ImportMode::Copy, "skills");
    assert!(again.iter().all(|r| r.status == SyncImportStatus::Skipped));
    assert_eq!(
        again
            .iter()
            .filter(|r| r.skip_reason == Some(SkipReason::TargetExists))
            .count(),
        4
    );
    assert_eq!(
        again
            .iter()
            .filter(|r| r.skip_reason == Some(SkipReason::SameNameExists))
            .count(),
        1
    );
}

// symlink semantics (unix); windows link modes are the recorded
// second-pass ACL/link work.
#[cfg(unix)]
#[test]
fn symlink_mode_keeps_okra_pointing_at_the_source() {
    let td = tempfile::tempdir().unwrap();
    let home = td.path();
    write_skill(&home.join(".claude/skills"), "review", "review", "claude skill");
    write_command(&home.join(".claude/commands"), "fix.md", "fix body\n");

    let svc = SettingsSyncService::new(home);
    let skills = svc.collect_skill_candidates(None);
    let results = svc.import(&skills, ImportMode::Symlink, "skills");
    assert!(results.iter().all(|r| r.status == SyncImportStatus::Imported));
    let link = home.join(".okra/skills/review");
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    // the linked SKILL.md resolves through the symlink
    assert!(link.join("SKILL.md").exists());

    let commands = svc.collect_command_candidates(None);
    let results = svc.import(&commands, ImportMode::Symlink, "commands");
    assert!(results.iter().all(|r| r.status == SyncImportStatus::Imported));
    let command_link = home.join(".okra/commands/fix.md");
    assert!(std::fs::symlink_metadata(&command_link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(command_link.exists());
}
