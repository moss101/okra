//! Plugin domain end-to-end (MASTER-PLAN §3 #46): a distributor signs a
//! data-only manifest bundle; a consumer with the publisher's key in its
//! trust store installs it content-addressed; a consumer without the key
//! refuses. Manifest surfaces load as DATA (skills, prompts, MCP
//! descriptors, hook definitions) — never executed code.

use okra_host::plugins::{
    install_signed, parse_manifest, publish_signed, verify_bundle, DiagnosticSeverity,
    PluginStore, SignatureVerdict, TrustStore,
};
use okra_host::plugins::generate_seed;

/// RFC 8032 §7.1 TEST 1 seed (fixed for reproducibility).
const PUBLISHER_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
    0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
    0x7f, 0x60,
];

const GOOD_MANIFEST: &str = r#"{
    "name": "okra-notes",
    "version": "2.0.1",
    "description": "Structured notes skills",
    "keywords": ["notes", "zettelkasten"],
    "author": "Ada Lovelace",
    "skills": ["./skills/notes", "./skills/review"],
    "commands": "./commands",
    "sessionStart": { "skill": "notes-welcome" },
    "systemPrompt": "Maintain the user's notes.",
    "hooks": [{ "event": "preToolUse", "matcher": "write_file" }],
    "mcpServers": {
        "notes-index": { "transport": "stdio", "command": "notes-index" }
    }
}"#;

#[test]
fn distributor_signs_consumer_installs() {
    let td = tempfile::tempdir().unwrap();
    let store = PluginStore::open(td.path().join("plugins"));

    // producer: sign the exact bytes and pin the content address
    let (receipt, envelope) = publish_signed(&store, PUBLISHER_SEED, GOOD_MANIFEST.as_bytes())
        .unwrap();
    assert_eq!(receipt.plugin_name.as_deref(), Some("okra-notes"));
    assert_eq!(receipt.sha256, envelope.plugin_sha256);

    // consumer: same bytes arrive over the wire
    let mut trust = TrustStore::new();
    trust.trust(&envelope.signer);
    let verdict =
        verify_bundle(GOOD_MANIFEST.as_bytes(), &envelope, &trust).unwrap();
    assert_eq!(verdict, SignatureVerdict::Trusted);
    let installed = install_signed(&store, GOOD_MANIFEST.as_bytes(), &envelope, &trust).unwrap();
    assert_eq!(installed, receipt);
    store.verify_installed(&installed.sha256).unwrap();

    // the installed manifest loads as data
    let parsed = parse_manifest(GOOD_MANIFEST);
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    let m = parsed.manifest.unwrap();
    assert_eq!(m.skills.len(), 2);
    assert_eq!(m.session_start.as_ref().unwrap().skill, "notes-welcome");
    assert_eq!(m.mcp_servers.as_ref().unwrap().len(), 1);
    assert_eq!(m.hooks.as_ref().unwrap().len(), 1);
}

#[test]
fn untrusted_or_unsigned_distribution_never_installs() {
    let td = tempfile::tempdir().unwrap();
    let store = PluginStore::open(td.path().join("plugins"));
    let (_receipt, envelope) = publish_signed(&store, PUBLISHER_SEED, GOOD_MANIFEST.as_bytes())
        .unwrap();

    // valid signature, unknown signer: refuses to install
    let err = install_signed(
        &store,
        GOOD_MANIFEST.as_bytes(),
        &envelope,
        &TrustStore::new(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("UnknownSigner"), "{err}");

    // same key, different bytes: digest failure before any write — the
    // tampered variant never gains its own content address
    let tampered = GOOD_MANIFEST.replace("2.0.1", "2.0.2-evil");
    let mut trust = TrustStore::new();
    trust.trust(&envelope.signer);
    let err = install_signed(&store, tampered.as_bytes(), &envelope, &trust).unwrap_err();
    assert!(err.to_string().contains("does not match"), "{err}");
    let tampered_sha = okra_host::plugins::sha256_hex(tampered.as_bytes());
    assert!(!store.root().join(&tampered_sha).join("install.json").exists());
}

#[test]
fn unsigned_install_path_still_pins_content() {
    // distributors without signing infrastructure can still install by
    // pinned sha256 — ZCode's minimum bar
    let td = tempfile::tempdir().unwrap();
    let store = PluginStore::open(td.path().join("plugins"));
    let receipt = store
        .install(GOOD_MANIFEST.as_bytes(), &okra_host::plugins::sha256_hex(GOOD_MANIFEST.as_bytes()), None)
        .unwrap();
    assert_eq!(receipt.signer, None);
    assert_eq!(receipt.plugin_name.as_deref(), Some("okra-notes"));
    store.verify_installed(&receipt.sha256).unwrap();
}

#[test]
fn generated_publisher_key_round_trips() {
    let seed = generate_seed().unwrap();
    let td = tempfile::tempdir().unwrap();
    let store = PluginStore::open(td.path().join("plugins"));
    let (receipt, envelope) = publish_signed(&store, seed, GOOD_MANIFEST.as_bytes()).unwrap();
    let mut trust = TrustStore::new();
    trust.trust(&envelope.signer);
    assert_eq!(
        verify_bundle(GOOD_MANIFEST.as_bytes(), &envelope, &trust).unwrap(),
        SignatureVerdict::Trusted
    );
    assert_eq!(receipt.size_bytes, GOOD_MANIFEST.len());
}

#[test]
fn manifest_degrades_gracefully_on_messy_fields() {
    let parsed = parse_manifest(
        r#"{
            "name": "messy_plugin",
            "skills": ["./skills"],
            "sessionStart": {},
            "mcpServers": "oops",
            "hooks": "oops",
            "tools": [{"native": true}]
        }"#,
    );
    let m = parsed.manifest.expect("only name errors abort");
    assert_eq!(m.session_start, None);
    assert_eq!(m.mcp_servers, None);
    assert_eq!(m.hooks, None);
    let severities: Vec<DiagnosticSeverity> = parsed
        .diagnostics
        .iter()
        .map(|d| d.severity)
        .collect();
    assert!(severities.contains(&DiagnosticSeverity::Warn));
    assert!(severities.contains(&DiagnosticSeverity::Info));
    assert!(!severities.contains(&DiagnosticSeverity::Error), "{severities:?}");
}
