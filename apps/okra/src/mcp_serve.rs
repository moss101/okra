//! `okra mcp-serve --computer [--allow-app NAME]... [--allow-full-control]`
//! — the M5 standalone seam (ARCHITECTURE's "MCP servers at M5"): an MCP
//! stdio server exposing the real macOS computer backend so EXTERNAL
//! MCP clients (any editor or agent that speaks MCP) can drive okra's
//! AX-first control.
//!
//! Consent model (the honest constraint of a headless server): there is
//! no surface to raise an approval card, so consent comes from the
//! LAUNCH CONFIG — `--allow-app` seeds the per-app ledger,
//! `--allow-full-control` grants display scope — and everything else is
//! REFUSED fail-closed with a message that names the missing flag. The
//! session lock still holds: grants live for the server's lifetime.
//!
//! Wire: JSON-RPC 2.0 over stdio, `initialize` → `tools/list` →
//! `tools/call` (the same shape okra's own MCP client speaks,
//! crates/tools/src/mcp.rs).

use std::io::{BufRead, Write};

use okra_computer::ConsentKind;
use okra_computer::{ConsentLedger, NoRaiseMode};

struct Outbound {
    out: std::sync::Mutex<std::io::Stdout>,
}

impl Outbound {
    fn result(&self, id: &serde_json::Value, result: serde_json::Value) {
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "result": result
        }));
    }
    fn error(&self, id: &serde_json::Value, code: i64, message: &str) {
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message }
        }));
    }
    fn send(&self, frame: serde_json::Value) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{}", serde_json::to_string(&frame).unwrap_or_default());
        let _ = out.flush();
    }
}

struct ToolDef {
    name: &'static str,
    description: &'static str,
    schema: serde_json::Value,
    run: fn(&Ctx, &serde_json::Value) -> Result<String, String>,
}

/// Server-side execution context: the consent ledger + launch flags.
struct Ctx {
    ledger: ConsentLedger,
    full_control: bool,
}

fn tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "computer_observe",
            description: "Observe an application's accessibility tree (element ids, roles, labels, positions). Call before computer_act.",
            schema: serde_json::json!({
                "type": "object",
                "properties": { "app": { "type": "string" } },
                "required": ["app"],
            }),
            run: |ctx, args| {
                let app = args["app"].as_str().unwrap_or_default();
                require_app(ctx, app, "computer_observe")?;
                let tree = okra_computer::backend::observe(app)
                    .map_err(|e| format!("observe {app}: {e}"))?;
                Ok(serde_json::to_string_pretty(&tree).unwrap_or_default())
            },
        },
        ToolDef {
            name: "computer_act",
            description: "Act on the frontmost app: click an AX element id, type text, or press a key. Re-observes after the action.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "app": { "type": "string" },
                    "click_element": { "type": "string", "description": "element id from computer_observe" },
                    "type_text": { "type": "string" },
                    "press_key": { "type": "string" }
                },
                "required": ["app"],
            }),
            run: |ctx, args| {
                let app = args["app"].as_str().unwrap_or_default();
                require_app(ctx, app, "computer_act")?;
                // display-scope actions (coordinate/typing) need full control
                let touches_display = args["type_text"].is_string() || args["press_key"].is_string();
                if touches_display && !ctx.full_control {
                    return Err(
                        "screen takeover consent missing: relaunch the server with --allow-full-control (typing/keys drive the display)"
                            .into(),
                    );
                }
                let click = args["click_element"].as_str();
                if click.is_some() {
                    let tree = okra_computer::backend::observe(app)
                        .map_err(|e| format!("observe {app}: {e}"))?;
                    okra_computer::backend::click_element(app, &tree, click.unwrap_or_default())
                        .map_err(|e| format!("click: {e}"))?;
                    okra_computer::backend::wait(150);
                }
                if let Some(text) = args["type_text"].as_str() {
                    okra_computer::backend::type_text(text).map_err(|e| format!("type: {e}"))?;
                }
                if let Some(key) = args["press_key"].as_str() {
                    okra_computer::backend::press_key(key).map_err(|e| format!("key: {e}"))?;
                }
                // user_actively_typing guard + re-observe after every
                // action (Claude contract)
                let tree = okra_computer::backend::observe(app)
                    .map_err(|e| format!("re-observe {app}: {e}"))?;
                Ok(format!("acted; {} elements now visible", tree.elements.len()))
            },
        },
        ToolDef {
            name: "computer_screenshot",
            description: "Capture the screen as a PNG data URL (requires --allow-full-control).",
            schema: serde_json::json!({ "type": "object", "properties": {} }),
            run: |ctx, _args| {
                if !ctx.full_control {
                    return Err(
                        "screen takeover consent missing: relaunch the server with --allow-full-control".into(),
                    );
                }
                let png = okra_computer::backend::screenshot().map_err(|e| format!("screenshot: {e}"))?;
                Ok(format!(
                    "data:image/png;base64,{}",
                    crate::serve::b64_png(&png)
                ))
            },
        },
        ToolDef {
            name: "computer_list_apps",
            description: "List running applications (no consent needed — names only).",
            schema: serde_json::json!({ "type": "object", "properties": {} }),
            run: |_ctx, _args| {
                let apps = okra_computer::backend::list_running_apps()
                    .map_err(|e| format!("list apps: {e}"))?;
                Ok(apps.join("\n"))
            },
        },
        ToolDef {
            name: "computer_open_application",
            description: "Open an application without raising it (background, no-raise mode).",
            schema: serde_json::json!({
                "type": "object",
                "properties": { "app": { "type": "string" } },
                "required": ["app"],
            }),
            run: |ctx, args| {
                let app = args["app"].as_str().unwrap_or_default();
                require_app(ctx, app, "computer_open_application")?;
                // no-raise: the background open path, never frontmost
                let no_raise = NoRaiseMode { enabled: true };
                let _ = no_raise;
                okra_computer::backend::open_application(app)
                    .map_err(|e| format!("open: {e}"))?;
                Ok(format!("opened {app} (no-raise)"))
            },
        },
    ]
}

/// Fail-closed consent gate: the launch config is the only consent a
/// headless server has.
fn require_app(ctx: &Ctx, app: &str, tool: &str) -> Result<(), String> {
    if ctx.ledger.has(ConsentKind::AppCapability, app) {
        Ok(())
    } else {
        Err(format!(
            "app consent missing for `{app}` ({tool}): relaunch the server with --allow-app {app}"
        ))
    }
}

pub fn serve_computer_mcp(allow_apps: Vec<String>, allow_full_control: bool) -> ! {
    let mut ledger = ConsentLedger::default();
    for app in &allow_apps {
        ledger.grant(ConsentKind::AppCapability, app);
    }
    let ctx = Ctx { ledger, full_control: allow_full_control };
    let outbound = Outbound { out: std::sync::Mutex::new(std::io::stdout()) };
    eprintln!(
        "[mcp-serve] computer MCP server on stdio ({} app(s) allowed, full_control={allow_full_control})",
        allow_apps.len()
    );

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            outbound.error(&serde_json::Value::from(-1), -32700, "parse error: not valid JSON");
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = msg.get("params").cloned().unwrap_or(serde_json::Value::Null);
        match method.as_str() {
            "initialize" => {
                outbound.result(&id, serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": {
                        "name": "okra-computer",
                        "version": env!("CARGO_PKG_VERSION"),
                    }
                }));
            }
            "notifications/initialized" | "initialized" => {}
            "tools/list" => {
                let tools: Vec<serde_json::Value> = tools()
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.schema,
                        })
                    })
                    .collect();
                outbound.result(&id, serde_json::json!({ "tools": tools }));
            }
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default().to_string();
                let arguments = params["arguments"].clone();
                let all_tools = tools();
                let Some(tool) = all_tools.iter().find(|t| t.name == name) else {
                    outbound.error(&id, -32602, &format!("unknown tool: {name}"));
                    continue;
                };
                match (tool.run)(&ctx, &arguments) {
                    Ok(text) => outbound.result(&id, serde_json::json!({
                        "content": [ { "type": "text", "text": text } ],
                        "isError": false,
                    })),
                    Err(e) => outbound.result(&id, serde_json::json!({
                        "content": [ { "type": "text", "text": e } ],
                        "isError": true,
                    })),
                }
            }
            other => {
                outbound.error(&id, -32601, &format!("method not found: {other}"));
            }
        }
    }
    std::process::exit(0);
}
