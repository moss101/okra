//! Host domain: i18n catalogs — the M6 slice (MASTER-PLAN §4 M6: "i18n
//! (port ZCode's en-US/zh-CN catalogs)"). The full 6.8k-key UI catalog
//! stays in the reused TS UI package (§3 #49 — the UI is a thin client,
//! not a rewrite); THIS module is the daemon-side localization contract
//! for strings surfaces render on the daemon's behalf:
//!
//! - **flat dotted keys → strings**, `{placeholder}` interpolation
//!   (the IntlProvider shape);
//! - **fallback chain**: a missing zh-CN key resolves to en-US (never a
//!   hard failure), an unknown key resolves to the key itself so callers
//!   can render before the catalog catches up;
//! - **locale negotiation** from `Accept-Language`-style tag lists
//!   (`zh-CN,en;q=0.8`), exact tag → base tag → default;
//! - the builtin catalog seeds with the first ported key set (exact
//!   en-US/zh-CN values from `packages/ui/src/i18n/locales/`), and
//!   `with_catalog` lets a deployment extend/override without a code
//!   change.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocaleTag {
    EnUs,
    ZhCn,
}

impl LocaleTag {
    pub fn as_str(self) -> &'static str {
        match self {
            LocaleTag::EnUs => "en-US",
            LocaleTag::ZhCn => "zh-CN",
        }
    }

    fn from_exact(tag: &str) -> Option<LocaleTag> {
        match tag.trim().to_ascii_lowercase().as_str() {
            "en-us" | "en_us" => Some(LocaleTag::EnUs),
            "zh-cn" | "zh_cn" | "zh-hans" => Some(LocaleTag::ZhCn),
            _ => None,
        }
    }

    /// Locale preference order for an Accept-Language-style list
    /// (`zh-CN,en-US;q=0.8` — q-values respected, order breaks ties).
    fn preference(accept: &str) -> Vec<LocaleTag> {
        let mut entries: Vec<(f32, LocaleTag, usize)> = Vec::new();
        for (i, part) in accept.split(',').enumerate() {
            let mut seg = part.splitn(2, ';');
            let tag = seg.next().unwrap_or_default().trim();
            let q = seg
                .next()
                .and_then(|params| {
                    params
                        .split(';')
                        .find(|p| p.trim().starts_with("q="))
                        .and_then(|p| p.trim()[2..].parse::<f32>().ok())
                })
                .unwrap_or(1.0);
            if q <= 0.0 {
                continue;
            }
            let exact = LocaleTag::from_exact(tag);
            let base = tag.split(['-', '_']).next().unwrap_or(tag);
            let base_match = match base {
                "en" => Some(LocaleTag::EnUs),
                "zh" => Some(LocaleTag::ZhCn),
                _ => None,
            };
            if let Some(l) = exact {
                entries.push((q, l, i));
            } else if let Some(l) = base_match {
                // base-tag matches rank below exact matches at equal q
                entries.push((q - 0.5, l, i));
            }
        }
        entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then(a.2.cmp(&b.2)));
        let mut out: Vec<LocaleTag> = Vec::new();
        for (_, l, _) in entries {
            if !out.contains(&l) {
                out.push(l);
            }
        }
        out
    }
}

/// Negotiate the serving locale (default en-US).
pub fn negotiate_locale(accept_language: &str) -> LocaleTag {
    LocaleTag::preference(accept_language)
        .into_iter()
        .next()
        .unwrap_or(LocaleTag::EnUs)
}

/// The daemon-side catalog: en-US is the source of truth; zh-CN overrides.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    en_us: BTreeMap<String, String>,
    zh_cn: BTreeMap<String, String>,
}

impl Catalog {
    /// The ported seed set (exact values from ZCode's locale files).
    pub fn builtin() -> Self {
        let en: &[(&str, &str)] = &[
            ("startPlan.recommendation.subagentDescription", "Your Start Plan has quota available for {model}. Switch this subagent’s model to the Start Plan?"),
            ("startPlan.recommendation.preferenceSaveFailed", "Could not save “Don’t ask again”. Continuing with your choice for this operation."),
            ("startPlan.recommendation.title", "Start Plan quota available"),
            ("startPlan.recommendation.description", "Your Start Plan still has quota for {model}. Would you like to use it?"),
            ("startPlan.recommendation.switch", "Switch plan"),
            ("startPlan.recommendation.decline", "Not now"),
            ("startPlan.recommendation.dismiss", "Don’t show again"),
            ("occupationOnboarding.skip", "Skip"),
            ("occupationOnboarding.start", "Get started"),
            ("occupationOnboarding.continue", "Next"),
            ("occupationOnboarding.saving", "Saving…"),
            ("occupationOnboarding.back", "Back"),
            ("occupationOnboarding.error", "Failed to save. Please try again."),
            ("chat.plugins.browseMarketplace", "Browse plugin marketplace"),
            ("chat.plugins.loadError", "Could not load plugins. Reopen the menu to try again."),
        ];
        let zh: &[(&str, &str)] = &[
            ("startPlan.recommendation.subagentDescription", "你的体验套餐中，{model} 仍有可用额度，是否将此子智能体的模型切换到体验套餐？"),
            ("startPlan.recommendation.preferenceSaveFailed", "未能保存“不再提示”，本次仍按你的选择继续。"),
            ("startPlan.recommendation.title", "体验套餐有可用额度"),
            ("startPlan.recommendation.description", "你的体验套餐中，{model} 仍有可用额度，是否切换使用？"),
            ("startPlan.recommendation.switch", "切换套餐"),
            ("startPlan.recommendation.decline", "不了"),
            ("startPlan.recommendation.dismiss", "不再提示"),
            ("occupationOnboarding.skip", "跳过"),
            ("occupationOnboarding.start", "开始使用"),
            ("occupationOnboarding.continue", "下一步"),
            ("occupationOnboarding.saving", "正在保存…"),
            ("occupationOnboarding.back", "返回"),
            ("occupationOnboarding.error", "保存失败，请重试。"),
            ("chat.plugins.browseMarketplace", "浏览插件市场"),
            ("chat.plugins.loadError", "插件加载失败，请重新打开菜单"),
        ];
        Catalog {
            en_us: en.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            zh_cn: zh.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    /// Extend/override from a deployment-loaded map (e.g. a fetched
    /// `{"zh-CN": {...}, "en-US": {...}}` JSON document).
    pub fn with_catalog(mut self, locale: LocaleTag, entries: BTreeMap<String, String>) -> Self {
        match locale {
            LocaleTag::EnUs => self.en_us.extend(entries),
            LocaleTag::ZhCn => self.zh_cn.extend(entries),
        }
        self
    }

    fn raw(&self, locale: LocaleTag, key: &str) -> Option<&str> {
        match locale {
            LocaleTag::ZhCn => self
                .zh_cn
                .get(key)
                .or_else(|| self.en_us.get(key)),
            LocaleTag::EnUs => self.en_us.get(key),
        }
        .map(String::as_str)
    }

    /// Resolve + interpolate. Fallback: zh-CN → en-US; unknown keys render
    /// as the key itself (a missing translation must not blank a surface).
    pub fn text(&self, locale: LocaleTag, key: &str, args: &[(&str, &str)]) -> String {
        let template = self.raw(locale, key).unwrap_or(key);
        let mut out = template.to_string();
        for (name, value) in args {
            out = out.replace(&format!("{{{name}}}"), value);
        }
        out
    }

    /// Every key the catalog knows (union across locales) — surfaces use
    /// this to enumerate, e.g. for available-commands style announcements.
    pub fn keys(&self) -> Vec<String> {
        let mut all: Vec<String> = self.en_us.keys().cloned().collect();
        for k in self.zh_cn.keys() {
            if !all.contains(k) {
                all.push(k.clone());
            }
        }
        all.sort();
        all
    }
}

/// Parse a catalog document: `{"en-US": {key: text}, "zh-CN": {…}}`.
/// One locale's flat key→text map.
pub type CatalogEntries = BTreeMap<String, String>;

pub fn parse_catalog_document(doc: &serde_json::Value) -> Result<Vec<(LocaleTag, CatalogEntries)>, String> {
    let Some(map) = doc.as_object() else {
        return Err("catalog document is not an object".into());
    };
    let mut out = Vec::new();
    for (tag, entries) in map {
        let locale = LocaleTag::from_exact(tag)
            .ok_or_else(|| format!("unsupported locale tag: {tag}"))?;
        let obj = entries
            .as_object()
            .ok_or_else(|| format!("{tag}: entries are not an object"))?;
        let mut converted = BTreeMap::new();
        for (k, v) in obj {
            let text = v
                .as_str()
                .ok_or_else(|| format!("{tag}:{k}: value is not a string"))?;
            converted.insert(k.clone(), text.to_string());
        }
        out.push((locale, converted));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn negotiation_prefers_exact_then_base_then_default() {
        assert_eq!(negotiate_locale("zh-CN"), LocaleTag::ZhCn);
        assert_eq!(negotiate_locale("en-US"), LocaleTag::EnUs);
        assert_eq!(negotiate_locale("zh"), LocaleTag::ZhCn);
        assert_eq!(negotiate_locale("zh-Hans"), LocaleTag::ZhCn);
        // q-values reorder; exact beats base at equal weight
        assert_eq!(negotiate_locale("en;q=0.8, zh-CN"), LocaleTag::ZhCn);
        assert_eq!(negotiate_locale("fr, zh-CN;q=0.3, en;q=0.9"), LocaleTag::EnUs);
        // unsupported languages fall through to the default
        assert_eq!(negotiate_locale("fr-FR, de"), LocaleTag::EnUs);
        assert_eq!(negotiate_locale(""), LocaleTag::EnUs);
        // q=0 excludes
        assert_eq!(negotiate_locale("zh-CN;q=0, en"), LocaleTag::EnUs);
    }

    #[test]
    fn catalog_values_match_the_ported_zcode_locales() {
        let c = Catalog::builtin();
        assert_eq!(
            c.text(LocaleTag::EnUs, "startPlan.recommendation.title", &[]),
            "Start Plan quota available"
        );
        assert_eq!(
            c.text(LocaleTag::ZhCn, "startPlan.recommendation.title", &[]),
            "体验套餐有可用额度"
        );
        assert_eq!(c.text(LocaleTag::ZhCn, "occupationOnboarding.skip", &[]), "跳过");
        assert_eq!(
            c.text(LocaleTag::ZhCn, "chat.plugins.loadError", &[]),
            "插件加载失败，请重新打开菜单"
        );
    }

    #[test]
    fn interpolation_and_fallback_chain() {
        let c = Catalog::builtin();
        // {model} interpolates in BOTH locales
        assert_eq!(
            c.text(
                LocaleTag::EnUs,
                "startPlan.recommendation.description",
                &[("model", "glm-5.3-flash")]
            ),
            "Your Start Plan still has quota for glm-5.3-flash. Would you like to use it?"
        );
        assert_eq!(
            c.text(
                LocaleTag::ZhCn,
                "startPlan.recommendation.description",
                &[("model", "glm-5.3-flash")]
            ),
            "你的体验套餐中，glm-5.3-flash 仍有可用额度，是否切换使用？"
        );

        // a key missing in zh-CN falls back to en-US, not to the key
        let mut partial = BTreeMap::new();
        partial.insert("demo.only.english".to_string(), "English only".to_string());
        let c2 = c.clone().with_catalog(LocaleTag::EnUs, partial);
        assert_eq!(c2.text(LocaleTag::ZhCn, "demo.only.english", &[]), "English only");

        // an unknown key renders as the key itself
        assert_eq!(c.text(LocaleTag::ZhCn, "no.such.key", &[]), "no.such.key");
    }

    #[test]
    fn catalog_documents_parse_and_extend() {
        let c = Catalog::builtin();
        let doc = json!({
            "zh-CN": { "occupationOnboarding.skip": "跳过这一步" },
            "en-US": { "demo.greeting": "Hello {name}" }
        });
        let parsed = parse_catalog_document(&doc).unwrap();
        let mut c2 = c;
        for (locale, entries) in parsed {
            c2 = c2.with_catalog(locale, entries);
        }
        // override wins, new keys land, unsupported tags are rejected
        assert_eq!(c2.text(LocaleTag::ZhCn, "occupationOnboarding.skip", &[]), "跳过这一步");
        assert_eq!(
            c2.text(LocaleTag::EnUs, "demo.greeting", &[("name", "okra")]),
            "Hello okra"
        );
        // unsupported locale tags and non-string values are contract errors
        assert!(parse_catalog_document(&json!({ "fr-FR": {} })).is_err());
        assert!(parse_catalog_document(&json!({ "en-US": { "k": 3 } })).is_err());

        // keys() enumerates the union, sorted
        let keys = Catalog::builtin().keys();
        assert_eq!(keys.len(), 15);
        assert!(keys.windows(2).all(|w| w[0] <= w[1]));
    }
}
