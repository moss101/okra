//! Client scenes (MASTER-PLAN §3 #48, from ZCode `client-scenes/`):
//! server-authored scene configs — the localized prompt-starter cards a
//! surface shows on a fresh session.
//!
//! Donor contracts kept:
//! - the wire shape is `{code, msg, data: [config]}`;
//! - a config is namespace + scene + options; each option has an id,
//!   type, localized contents, optional prompts, items with labels and
//!   finish events, `refer` for cascades, and `cascades` mapping a
//!   parent option id to the filtered children;
//! - localization is a map keyed by language tag — the server owns
//!   translations, the client only picks one;
//! - okra adds an offline catalog served through the same lookup.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SceneItem {
    pub id: String,
    pub item_type: String,
    pub contents: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub descs: BTreeMap<String, String>,
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_finish: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub img: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SceneOption {
    pub id: String,
    pub option_type: String,
    pub contents: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prompts: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<SceneItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refer: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub cascades: BTreeMap<String, Vec<SceneItem>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub templates: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SceneConfig {
    pub namespace: String,
    pub scene: String,
    pub options: BTreeMap<String, SceneOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<u64>,
}

/// Localized string fallback: exact tag → base tag → English → first.
pub fn localized<'a>(map: &'a BTreeMap<String, String>, lang: &str) -> Option<&'a str> {
    if let Some(v) = map.get(lang) {
        return Some(v.as_str());
    }
    let base = lang.split('-').next().unwrap_or(lang);
    if let Some(v) = map.get(base) {
        return Some(v.as_str());
    }
    if let Some(v) = map.get("en") {
        return Some(v.as_str());
    }
    map.values().next().map(String::as_str)
}

/// Cascade: the items shown for an option after a parent value was chosen.
pub fn cascaded_items(option: &SceneOption, parent_value: &str) -> Vec<SceneItem> {
    option
        .cascades
        .get(parent_value)
        .cloned()
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub struct ClientSceneCatalog {
    scenes: Vec<SceneConfig>,
}

impl ClientSceneCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, config: SceneConfig) {
        self.scenes.push(config);
    }

    /// Server wire shape: `{code, msg, data: [config]}`.
    pub fn to_response_body(&self) -> Value {
        serde_json::json!({
            "code": 0,
            "msg": "ok",
            "data": self.scenes,
        })
    }

    pub fn scenes(&self) -> &[SceneConfig] {
        &self.scenes
    }
}

/// Parse a server wire body into scene configs (the `readApiJson` shape),
/// so an offline catalog and a fetched payload share one type.
pub fn parse_response_body(body: &Value) -> Result<Vec<SceneConfig>, String> {
    let data = body
        .get("data")
        .ok_or_else(|| "response missing data".to_string())?;
    serde_json::from_value(data.clone()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn lang_option() -> SceneOption {
        let mut contents = BTreeMap::new();
        contents.insert("en".into(), "Language".into());
        contents.insert("zh-CN".into(), "语言".into());
        SceneOption {
            id: "language".into(),
            option_type: "select".into(),
            contents,
            prompts: BTreeMap::new(),
            items: Vec::new(),
            refer: None,
            cascades: BTreeMap::new(),
            templates: BTreeMap::new(),
        }
    }

    #[test]
    fn localized_picks_tag_base_then_english_then_first() {
        let mut m = BTreeMap::new();
        m.insert("zh-CN".into(), "你好".into());
        m.insert("en".into(), "hello".into());
        assert_eq!(localized(&m, "zh-CN"), Some("你好"));
        assert_eq!(localized(&m, "en-US"), Some("hello"), "base tag fallback");
        assert_eq!(localized(&m, "fr"), Some("hello"), "english fallback");

        let mut only_ja = BTreeMap::new();
        only_ja.insert("ja".into(), "こんにちは".into());
        assert_eq!(localized(&only_ja, "zh"), Some("こんにちは"), "last resort: first value");
        assert_eq!(localized(&BTreeMap::new(), "en"), None);
    }

    #[test]
    fn parse_reads_the_wire_shape() {
        let option = lang_option();
        let body = json!({
            "code": 0,
            "msg": "ok",
            "data": [{
                "namespace": "starter",
                "scene": "welcome",
                "options": { "language": option },
            }]
        });
        let configs = parse_response_body(&body).unwrap();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].namespace, "starter");
        assert!(configs[0].options.contains_key("language"));
        assert!(parse_response_body(&json!({})).is_err());
    }

    #[test]
    fn catalog_serves_the_wire_shape() {
        let mut catalog = ClientSceneCatalog::new();
        catalog.register(SceneConfig {
            namespace: "starter".into(),
            scene: "welcome".into(),
            options: BTreeMap::from([("language".into(), lang_option())]),
            created_at: None,
            updated_at: None,
        });
        let body = catalog.to_response_body();
        assert_eq!(body["code"], json!(0));
        assert_eq!(body["data"][0]["scene"], json!("welcome"));
        let _ = &body;
        let parsed = parse_response_body(&body).unwrap();
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn cascades_filter_items_by_parent_value() {
        let mut cascades = BTreeMap::new();
        cascades.insert(
            "rust".into(),
            vec![SceneItem {
                id: "axum".into(),
                item_type: "option".into(),
                contents: BTreeMap::new(),
                descs: BTreeMap::new(),
                labels: BTreeMap::new(),
                on_finish: None,
                img: None,
            }],
        );
        cascades.insert(
            "typescript".into(),
            vec![SceneItem {
                id: "fastify".into(),
                item_type: "option".into(),
                contents: BTreeMap::new(),
                descs: BTreeMap::new(),
                labels: BTreeMap::new(),
                on_finish: None,
                img: None,
            }],
        );
        let option = SceneOption {
            id: "framework".into(),
            option_type: "select".into(),
            contents: BTreeMap::new(),
            cascades,
            ..Default::default()
        };
        assert_eq!(cascaded_items(&option, "rust")[0].id, "axum");
        assert_eq!(cascaded_items(&option, "typescript")[0].id, "fastify");
        assert!(cascaded_items(&option, "python").is_empty());
    }
}
