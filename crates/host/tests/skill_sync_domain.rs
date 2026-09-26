//! Skill-sync domain end-to-end (MASTER-PLAN §3 #48, from ZCode
//! skill-sync): a local machine scans its user skill roots (okra root
//! shadowing the agents interop root), exports a selected set as a
//! gzip'd ustar archive, and a remote machine imports it — skipping
//! same-name skills, containing paths, and bounding bytes.

use okra_host::skill_sync::{
    ImportOutcome, ImportStatus, SkillRoot, SkillSyncService, DEFAULT_MAX_ARCHIVE_BYTES,
    MAX_SKILL_SCAN_DEPTH, SKILL_FILE_NAME,
};
use std::path::Path;

fn write_skill(root: &Path, rel: &str, name: &str, description: &str, extra: &[(&str, &str)]) {
    let dir = root.join(rel);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(SKILL_FILE_NAME),
        format!("---\nname: {name}\ndescription: {description}\n---\n\nUse {name} well.\n"),
    )
    .unwrap();
    for (file, content) in extra {
        std::fs::write(dir.join(file), content).unwrap();
    }
}

#[test]
fn full_local_export_remote_import_flow() {
    let local = tempfile::tempdir().unwrap();
    write_skill(
        &local.path().join(".okra/skills"),
        "notes",
        "notes",
        "okra notes skill",
        &[("reference.md", "# Reference\n")],
    );
    write_skill(
        &local.path().join(".okra/skills"),
        "group/alpha",
        "alpha",
        "grouped skill",
        &[],
    );
    // agents interop root: one shadowed by name, one unique
    write_skill(&local.path().join(".agents/skills"), "alpha-dup", "ALPHA", "name shadowed", &[]);
    write_skill(&local.path().join(".agents/skills"), "beta", "beta", "agents only", &[]);

    let local_svc = SkillSyncService::new(local.path());
    let candidates = local_svc.candidates();
    let summary: Vec<(String, String)> = candidates
        .iter()
        .map(|c| {
            let root_name = match c.root {
                SkillRoot::Okra => "okra",
                SkillRoot::Agents => "agents",
            };
            (c.directory_name.clone(), root_name.to_string())
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            ("beta".to_string(), "agents".to_string()),
            ("group/alpha".to_string(), "okra".to_string()),
            ("notes".to_string(), "okra".to_string()),
        ],
        "name-shadowed agents skill excluded, rest sorted by directory"
    );

    // export all three
    let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
    let (archive, exported) = local_svc.export_archive(&ids).unwrap();
    assert_eq!(exported.len(), 3);
    assert!(archive.len() < DEFAULT_MAX_ARCHIVE_BYTES);
    // archive is real gzip (magic) — byte-compatible with the donor format
    assert_eq!(&archive[..2], &[0x1f, 0x8b]);

    // remote: has "alpha" already, and a junk entry that is no skill
    let remote = tempfile::tempdir().unwrap();
    write_skill(&remote.path().join(".okra/skills"), "group/alpha", "alpha", "remote original", &[]);
    std::fs::create_dir_all(remote.path().join(".okra/skills/junk")).unwrap();
    std::fs::write(remote.path().join(".okra/skills/junk/README"), "not a skill").unwrap();
    let remote_svc = SkillSyncService::new(remote.path());
    let outcomes: Vec<ImportOutcome> = remote_svc.import_archive(&archive).unwrap();
    let by_dir: Vec<(&str, ImportStatus)> = outcomes
        .iter()
        .map(|o| (o.directory_name.as_str(), o.status.clone()))
        .collect();
    assert_eq!(
        by_dir,
        vec![
            ("beta", ImportStatus::Synced),
            ("group/alpha", ImportStatus::Skipped),
            ("notes", ImportStatus::Synced),
        ]
    );

    // synced content landed whole
    let notes = remote.path().join(".okra/skills/notes");
    assert!(notes.join("SKILL.md").exists());
    assert!(notes.join("reference.md").exists());
    let alpha_content =
        std::fs::read_to_string(remote.path().join(".okra/skills/group/alpha/SKILL.md")).unwrap();
    assert!(
        alpha_content.contains("remote original"),
        "the same-name skill was skipped, not overwritten"
    );
    // the remote scan now sees beta + notes (okra) and the local alpha
    let remote_candidates = remote_svc.candidates();
    let remote_names: Vec<String> =
        remote_candidates.iter().map(|c| c.name.clone()).collect();
    assert!(remote_names.contains(&"beta".to_string()));
    assert!(remote_names.contains(&"notes".to_string()));
}

#[test]
fn skill_depth_limit_and_archive_caps_hold() {
    let td = tempfile::tempdir().unwrap();
    // build a chain deeper than the scan limit
    let mut rel = String::from("a");
    for _ in 0..(MAX_SKILL_SCAN_DEPTH + 2) {
        rel.push_str("/deep");
    }
    write_skill(&td.path().join(".okra/skills"), &rel, "abyss", "too deep to matter", &[]);
    let svc = SkillSyncService::new(td.path());
    assert!(svc.candidates().is_empty(), "beyond-depth skills are invisible");

    // a tiny archive cap rejects otherwise-fine exports
    let rich = tempfile::tempdir().unwrap();
    write_skill(&rich.path().join(".okra/skills"), "big", "big", "big skill", &[
        ("blob.bin", &"x".repeat(4096)),
    ]);
    let svc = SkillSyncService::new(rich.path());
    let id = svc.candidates()[0].id.clone();
    let capped = svc.clone().with_max_archive_bytes(16);
    assert!(matches!(
        capped.export_archive(&[id]),
        Err(okra_host::skill_sync::SkillSyncError::SizeLimit { .. })
    ));
}
