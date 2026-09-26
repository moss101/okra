//! Model-provider registry (MASTER-PLAN §3 #48, from ZCode
//! `packages/provider/` + `services/model-provider/`): the host-side
//! source of truth for which providers/models exist, which are
//! executable, and which selection is actually effective for a turn.
//!
//! Contracts ported from the TS sources:
//! - **owned ordering** (`owned-order.ts`): unsorted builtin members stay
//!   BEFORE the user's ordering, unsorted personal members AFTER it — so a
//!   remotely-added builtin never gets shuffled to the tail by the next
//!   personal write;
//! - **per-field overlays**: personal and account snapshots are partial
//!   (`ProviderOverlay`); only set fields replace the builtin base;
//! - **fail-closed account snapshots** (`sources.ts`): when the account
//!   source cannot resolve, every account-type provider is overlaid with
//!   `entitled: false` — unknown entitlement must block, never assume;
//! - **builtin-revision recovery** (`providerRuntime.ts`): a config
//!   builtin revision the account snapshot is not based on demands an
//!   account refresh (`builtin-account-recovery`);
//! - **executable vs selectable**: an account provider no longer has a
//!   master disable switch; a non-current account keeps its settings
//!   entry but publishes no models; `visibility: hidden` removes models
//!   from selection without deleting them;
//! - **selection completion** (`model-selection-config.ts`): a configured
//!   default is honored only when selectable; fresh initialization
//!   completes with the HIGHEST reasoning level; re-resolution of an
//!   existing selection normalizes (drops stale options) instead of
//!   silently inventing one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type ProviderId = str;
pub type ModelId = str;

// ---------------------------------------------------------------------------
// config model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Access {
    ApiKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key_management_url: Option<String>,
    },
    Account {
        account_type: String,
        mode: String,
        entitled: bool,
    },
}

impl Access {
    pub fn is_account(&self) -> bool {
        matches!(self, Access::Account { .. })
    }

    pub fn entitled(&self) -> bool {
        match self {
            Access::ApiKey { .. } => true,
            Access::Account { entitled, .. } => *entitled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiConfig {
    pub kind: String,
    pub base_url: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Visibility {
    #[default]
    Visible,
    Hidden,
}

/// Full builtin provider config. Partial mutations arrive as
/// [`ProviderOverlay`]s, never by rewriting this in place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default)]
    pub group: String,
    pub access: Access,
    pub api: ApiConfig,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builtin_model_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub personal_model_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_order: Vec<String>,
    #[serde(default)]
    pub visibility: Visibility,
    /// Master switch; account-type providers ignore an explicit `false`
    /// (they are gated by entitlement + connection instead).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl ProviderConfig {
    pub fn is_enabled(&self) -> bool {
        if self.access.is_account() {
            return true;
        }
        self.enabled.unwrap_or(true)
    }

    /// Empty base for personal-only providers: the overlay must supply
    /// access + api, or resolution fails closed (no executable models).
    pub fn empty() -> Self {
        Self {
            group: String::new(),
            access: Access::ApiKey { api_key: None, api_key_management_url: None },
            api: ApiConfig { kind: String::new(), base_url: String::new(), headers: BTreeMap::new() },
            builtin_model_ids: Vec::new(),
            personal_model_ids: Vec::new(),
            model_order: Vec::new(),
            visibility: Visibility::Visible,
            enabled: None,
        }
    }
}

/// Per-field partial: only `Some` fields replace the base config.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderOverlay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<Access>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<ApiConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_model_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personal_model_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_order: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

impl ProviderOverlay {
    /// Fail-closed account overlay: same account identity, entitlement
    /// explicitly revoked.
    pub fn unentitled(base: &Access) -> Self {
        let Access::Account { account_type, mode, .. } = base else {
            return Self::default();
        };
        Self {
            access: Some(Access::Account {
                account_type: account_type.clone(),
                mode: mode.clone(),
                entitled: false,
            }),
            ..Self::default()
        }
    }

    pub fn apply_to(&self, base: ProviderConfig) -> ProviderConfig {
        let mut c = base;
        if let Some(v) = &self.group {
            c.group = v.clone();
        }
        if let Some(v) = &self.access {
            c.access = v.clone();
        }
        if let Some(v) = &self.api {
            c.api = v.clone();
        }
        if let Some(v) = &self.builtin_model_ids {
            c.builtin_model_ids = v.clone();
        }
        if let Some(v) = &self.personal_model_ids {
            c.personal_model_ids = v.clone();
        }
        if let Some(v) = &self.model_order {
            c.model_order = v.clone();
        }
        if let Some(v) = &self.visibility {
            c.visibility = *v;
        }
        if self.enabled.is_some() {
            c.enabled = self.enabled;
        }
        c
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub enabled: bool,
    pub context_window: u64,
    pub supports_tool_call: bool,
    /// `optionSpecs.reasoningLevel.values` — order matters: the LAST
    /// entry is the highest level, used for fresh-selection completion.
    #[serde(default)]
    pub reasoning_levels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// Effective per-provider model configs (builtin + personal already
/// composed by the caller, mirroring `ModelConfigRules.composeEffective`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelCatalog(pub BTreeMap<String, BTreeMap<String, ModelConfig>>);

// ---------------------------------------------------------------------------
// sources
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshot {
    pub revision: String,
    pub zcode_builtin_revision: String,
    pub zcode_builtin_providers: BTreeMap<String, ProviderConfig>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub personal_providers: BTreeMap<String, ProviderOverlay>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub personal_provider_order: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountState {
    /// `None` = unknown (treated as current); `Some(false)` = a known
    /// non-current connection keeps its settings entry but publishes no
    /// models into the registry.
    pub current: Option<bool>,
}

impl AccountState {
    pub fn is_current(&self) -> bool {
        self.current != Some(false)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSnapshot {
    pub revision: String,
    pub based_on_zcode_builtin_revision: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderOverlay>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub states: BTreeMap<String, AccountState>,
}

/// The account source could not resolve: every builtin account-type
/// provider is explicitly unentitled (fail closed — unknown must block).
pub fn fail_closed_account(config: &ConfigSnapshot) -> AccountSnapshot {
    let providers = config
        .zcode_builtin_providers
        .iter()
        .filter(|(_, p)| p.access.is_account())
        .map(|(id, p)| (id.clone(), ProviderOverlay::unentitled(&p.access)))
        .collect();
    AccountSnapshot {
        revision: format!("account:fail-closed:{}", config.zcode_builtin_revision),
        based_on_zcode_builtin_revision: config.zcode_builtin_revision.clone(),
        providers,
        states: BTreeMap::new(),
    }
}

/// The config's builtin revision moved past what the account snapshot was
/// resolved against → the account source must be refreshed
/// (`builtin-account-recovery`). A missing account snapshot always counts.
pub fn needs_account_recovery(
    builtin_revision: &str,
    account: Option<&AccountSnapshot>,
) -> bool {
    match account {
        None => true,
        Some(a) => a.based_on_zcode_builtin_revision != builtin_revision,
    }
}

// ---------------------------------------------------------------------------
// owned ordering (port of owned-order.ts)
// ---------------------------------------------------------------------------

fn unique_in_order(ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    ids.iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect()
}

/// Unsorted builtin members stay BEFORE the user ordering, unsorted
/// personal members AFTER it. Reordering a remote builtin addition to the
/// tail would otherwise poison the next personal write.
pub fn resolve_owned_order(
    builtin_ids: &[String],
    personal_ids: &[String],
    requested_order: &[String],
) -> Vec<String> {
    let builtin = unique_in_order(builtin_ids);
    let builtin_set: std::collections::BTreeSet<_> = builtin.iter().cloned().collect();
    let personal: Vec<_> = unique_in_order(personal_ids)
        .into_iter()
        .filter(|id| !builtin_set.contains(id))
        .collect();
    let members: std::collections::BTreeSet<String> =
        builtin.iter().chain(personal.iter()).cloned().collect();
    let ordered: Vec<_> = unique_in_order(requested_order)
        .into_iter()
        .filter(|id| members.contains(id))
        .collect();
    let ordered_set: std::collections::BTreeSet<_> = ordered.iter().cloned().collect();
    builtin
        .iter()
        .filter(|id| !ordered_set.contains(*id))
        .cloned()
        .chain(ordered)
        .chain(personal.iter().filter(|id| !ordered_set.contains(*id)).cloned())
        .collect()
}

// ---------------------------------------------------------------------------
// registry resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelSource {
    Builtin,
    Personal,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryModel {
    pub model_id: String,
    pub source: ModelSource,
    pub enabled: bool,
    /// Provider gates passed AND model enabled AND config present.
    pub executable: bool,
    /// Executable AND provider visible — may appear in selection UI.
    pub selectable: bool,
    pub config: ModelConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryProvider {
    pub provider_id: String,
    pub enabled: bool,
    pub entitled: bool,
    pub account_current: bool,
    pub executable: bool,
    pub config: ProviderConfig,
    pub models: Vec<RegistryModel>,
}

impl RegistryProvider {
    pub fn model(&self, model_id: &ModelId) -> Option<&RegistryModel> {
        self.models.iter().find(|m| m.model_id == model_id)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryView {
    pub revision: u64,
    pub providers: Vec<RegistryProvider>,
}

impl RegistryView {
    pub fn provider(&self, provider_id: &ProviderId) -> Option<&RegistryProvider> {
        self.providers.iter().find(|p| p.provider_id == provider_id)
    }

    pub fn model(&self, provider_id: &ProviderId, model_id: &ModelId) -> Option<&RegistryModel> {
        self.provider(provider_id)?.model(model_id)
    }
}

/// Resolve builtin + personal + account sources into the published view.
///
/// Order: builtin map order first (BTreeMap = canonical lexicographic),
/// personal-only providers after, per `resolve_owned_order`. Each provider
/// is gated: `executable = enabled && entitled && account_current &&
/// api.base_url present`; each model additionally needs `enabled` and a
/// catalog entry; `selectable` additionally needs `visibility != hidden`.
pub fn resolve(
    config: &ConfigSnapshot,
    account: Option<&AccountSnapshot>,
    catalog: &ModelCatalog,
) -> RegistryView {
    let empty_states: BTreeMap<String, AccountState> = BTreeMap::new();
    let states = account.map(|a| &a.states).unwrap_or(&empty_states);
    let overlays = |id: &str| -> ProviderOverlay {
        let mut merged = ProviderOverlay::default();
        if let Some(p) = config.personal_providers.get(id) {
            merged = p.clone();
        }
        if let Some(a) = account.and_then(|a| a.providers.get(id)) {
            merged = overlay_merge(&merged, a);
        }
        merged
    };

    let mut ids: Vec<String> = config.zcode_builtin_providers.keys().cloned().collect();
    let builtin_set: std::collections::BTreeSet<_> = ids.iter().cloned().collect();
    let personal_only: Vec<String> = config
        .personal_providers
        .keys()
        .filter(|id| !builtin_set.contains(*id))
        .cloned()
        .collect();
    ids.extend(resolve_owned_order(&[], &personal_only, &config.personal_provider_order));

    let providers = ids
        .iter()
        .map(|id| {
            // personal-only providers assemble over an empty base; their
            // overlay must supply access + api or validation fails closed
            let base = config
                .zcode_builtin_providers
                .get(id)
                .cloned()
                .unwrap_or_else(ProviderConfig::empty);
            let cfg = overlays(id).apply_to(base);
            (id.clone(), cfg)
        })
        .map(|(id, cfg)| assemble_provider(id, cfg, states, catalog))
        .collect();

    RegistryView { revision: 0, providers }
}

/// Overlay composition: later wins per field, `None` fields pass through.
fn overlay_merge(base: &ProviderOverlay, next: &ProviderOverlay) -> ProviderOverlay {
    ProviderOverlay {
        group: next.group.clone().or_else(|| base.group.clone()),
        access: next.access.clone().or_else(|| base.access.clone()),
        api: next.api.clone().or_else(|| base.api.clone()),
        builtin_model_ids: next.builtin_model_ids.clone().or(base.builtin_model_ids.clone()),
        personal_model_ids: next.personal_model_ids.clone().or(base.personal_model_ids.clone()),
        model_order: next.model_order.clone().or(base.model_order.clone()),
        visibility: next.visibility.or(base.visibility),
        enabled: next.enabled.or(base.enabled),
    }
}

fn assemble_provider(
    id: String,
    cfg: ProviderConfig,
    states: &BTreeMap<String, AccountState>,
    catalog: &ModelCatalog,
) -> RegistryProvider {
    let enabled = cfg.is_enabled();
    let entitled = cfg.access.entitled();
    let account_current = states.get(&id).is_none_or(AccountState::is_current);
    let executable_provider = enabled && entitled && account_current && !cfg.api.base_url.is_empty();

    let ordered = resolve_owned_order(
        &cfg.builtin_model_ids,
        &cfg.personal_model_ids,
        &cfg.model_order,
    );
    let builtin: std::collections::BTreeSet<_> = unique_in_order(&cfg.builtin_model_ids)
        .into_iter()
        .collect();

    let models = ordered
        .into_iter()
        .map(|model_id| {
            let source = if builtin.contains(&model_id) {
                ModelSource::Builtin
            } else {
                ModelSource::Personal
            };
            let config = catalog
                .0
                .get(&id)
                .and_then(|m| m.get(&model_id))
                .cloned();
            let model_enabled = config.as_ref().is_some_and(|c| c.enabled);
            let executable = executable_provider && model_enabled && config.is_some();
            let selectable = executable && cfg.visibility != Visibility::Hidden;
            RegistryModel {
                model_id,
                source,
                enabled: model_enabled,
                executable,
                selectable,
                config: config.unwrap_or(ModelConfig {
                    enabled: false,
                    context_window: 0,
                    supports_tool_call: false,
                    reasoning_levels: Vec::new(),
                    max_output_tokens: None,
                }),
            }
        })
        .collect();

    RegistryProvider {
        provider_id: id,
        enabled,
        entitled,
        account_current,
        executable: executable_provider,
        config: cfg,
        models,
    }
}

// ---------------------------------------------------------------------------
// selection semantics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SelectionOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_level: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub provider_id: String,
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<SelectionOptions>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SelectionIssue {
    #[error("provider not found: {0}")]
    ProviderNotFound(String),
    #[error("model not found: {0}/{1}")]
    ModelNotFound(String, String),
    #[error("model is disabled: {0}/{1}")]
    ModelDisabled(String, String),
    #[error("reasoning level missing")]
    ReasoningLevelMissing,
    #[error("reasoning level not supported (supported: {0})")]
    ReasoningLevelNotSupported(String),
    #[error("account connection unavailable")]
    AccountConnectionUnavailable,
    #[error("selection missing")]
    SelectionMissing,
}

/// Validate a selection's options against the model's option specs
/// (`validateModelSelectionOptions`): a reasoning level is REQUIRED and
/// must be one of the model's published values.
pub fn validate_options(
    provider_id: &ProviderId,
    model_id: &ModelId,
    model: &RegistryModel,
    options: Option<&SelectionOptions>,
) -> Result<(), SelectionIssue> {
    let level = options
        .and_then(|o| o.reasoning_level.as_deref())
        .ok_or(SelectionIssue::ReasoningLevelMissing)?;
    if !model.config.reasoning_levels.iter().any(|v| v == level) {
        return Err(SelectionIssue::ReasoningLevelNotSupported(
            model.config.reasoning_levels.join(", "),
        ));
    }
    let _ = (provider_id, model_id);
    Ok(())
}

/// Registry-level selection validation: existence first, then options.
pub fn validate_selection(view: &RegistryView, sel: &ModelSelection) -> Result<(), SelectionIssue> {
    let provider = view
        .provider(&sel.provider_id)
        .ok_or_else(|| SelectionIssue::ProviderNotFound(sel.provider_id.clone()))?;
    let model = provider
        .model(&sel.model_id)
        .ok_or_else(|| SelectionIssue::ModelNotFound(sel.provider_id.clone(), sel.model_id.clone()))?;
    validate_options(&sel.provider_id, &sel.model_id, model, sel.options.as_ref())
}

/// How a provider participates in selection resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    Ordinary,
    AccountPlan,
    /// Hidden by default, exempt from hidden-masking when addressed
    /// directly (off-peak accounts keep their own scheduling).
    AccountOffpeak,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveSelection {
    /// `None` when the selection cannot be resolved at all.
    pub selection: Option<ModelSelection>,
    /// Set together with a non-None selection when it resolved only
    /// partially (e.g. stale reasoning level).
    pub issue: Option<SelectionIssue>,
}

/// Resolve the effective selection for an upcoming turn
/// (`resolveEffectiveModelSelection`): account-plan indirection to the
/// CURRENT connected account, hidden masking, existence checks, and
/// strict option validation. Existing selections are parsed, never
/// silently repaired — a stale reasoning level is surfaced, not invented.
pub fn resolve_effective(
    view: &RegistryView,
    states: &BTreeMap<String, AccountState>,
    classify: &dyn Fn(&str) -> ProviderKind,
    selection: Option<&ModelSelection>,
) -> EffectiveSelection {
    let Some(original) = selection else {
        return EffectiveSelection { selection: None, issue: Some(SelectionIssue::SelectionMissing) };
    };
    let kind = classify(&original.provider_id);
    let mut provider_id = original.provider_id.clone();
    if kind == ProviderKind::AccountPlan {
        let current: Vec<&String> = states
            .iter()
            .filter(|(id, s)| {
                s.current == Some(true) && classify((*id).as_str()) == ProviderKind::AccountPlan
            })
            .map(|(id, _)| id)
            .collect();
        if current.len() != 1 {
            return EffectiveSelection {
                selection: None,
                issue: Some(SelectionIssue::AccountConnectionUnavailable),
            };
        }
        provider_id = current[0].clone();
    }
    let Some(provider) = view.provider(&provider_id) else {
        return EffectiveSelection {
            selection: None,
            issue: Some(SelectionIssue::ProviderNotFound(provider_id)),
        };
    };
    if provider.config.visibility == Visibility::Hidden && kind != ProviderKind::AccountOffpeak {
        return EffectiveSelection {
            selection: None,
            issue: Some(SelectionIssue::ProviderNotFound(provider_id)),
        };
    }
    let Some(model) = provider.model(&original.model_id) else {
        return EffectiveSelection {
            selection: None,
            issue: Some(SelectionIssue::ModelNotFound(
                provider_id,
                original.model_id.clone(),
            )),
        };
    };
    match validate_options(&provider_id, &original.model_id, model, original.options.as_ref()) {
        Ok(()) => EffectiveSelection {
            selection: Some(ModelSelection {
                provider_id,
                model_id: original.model_id.clone(),
                options: original.options.clone(),
            }),
            issue: None,
        },
        Err(issue @ (SelectionIssue::ReasoningLevelMissing
        | SelectionIssue::ReasoningLevelNotSupported(_))) => EffectiveSelection {
            selection: Some(ModelSelection {
                provider_id,
                model_id: original.model_id.clone(),
                options: None,
            }),
            issue: Some(issue),
        },
        Err(_) => unreachable!("validate_options only fails on reasoning levels here"),
    }
}

/// Highest published reasoning level (fresh selections take the top).
fn highest_reasoning_level(model: &RegistryModel) -> Option<String> {
    model.config.reasoning_levels.last().cloned()
}

/// Complete a NEW selection (user picked a model / fresh initialization):
/// keep the identity, fill in the highest reasoning level. `None` when the
/// model publishes no levels.
pub fn complete_new_selection(
    view: &RegistryView,
    provider_id: &ProviderId,
    model_id: &ModelId,
) -> Option<ModelSelection> {
    let model = view.model(provider_id, model_id)?;
    let reasoning_level = highest_reasoning_level(model)?;
    Some(ModelSelection {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        options: Some(SelectionOptions {
            reasoning_level: Some(reasoning_level),
        }),
    })
}

/// Whether a configured default can be honored as-is: visible provider,
/// existing model, valid options.
fn is_selectable(view: &RegistryView, sel: &ModelSelection) -> bool {
    let Some(provider) = view.provider(&sel.provider_id) else {
        return false;
    };
    if provider.config.visibility == Visibility::Hidden {
        return false;
    }
    let Some(model) = provider.model(&sel.model_id) else {
        return false;
    };
    validate_options(&sel.provider_id, &sel.model_id, model, sel.options.as_ref()).is_ok()
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "kebab-case")]
pub enum InitialSelection {
    ConfiguredDefault(ModelSelection),
    RegistryFallback(ModelSelection),
    None,
}

/// Host recommendation for a NEW draft (`resolveInitialModelSelection`):
/// the configured default while it stays selectable, else walk the
/// registry in order (skipping hidden providers) and complete the first
/// executable model with its highest reasoning level, else nothing. A
/// stale default is a discardable preference — it must not pin the user
/// to a dead model.
pub fn resolve_initial(
    view: &RegistryView,
    configured_default: Option<&ModelSelection>,
) -> InitialSelection {
    if let Some(def) = configured_default
        && is_selectable(view, def)
    {
        return InitialSelection::ConfiguredDefault(def.clone());
    }
    for provider in &view.providers {
        if provider.config.visibility == Visibility::Hidden {
            continue;
        }
        for model in &provider.models {
            if !model.selectable {
                continue;
            }
            if let Some(sel) =
                complete_new_selection(view, &provider.provider_id, &model.model_id)
            {
                return InitialSelection::RegistryFallback(sel);
            }
        }
    }
    InitialSelection::None
}

/// Normalize a selection about to be committed (`normalizeModelSelection`):
/// keep the model identity, DROP options that no longer validate — never
/// invent a level. `None` when the model itself is gone.
pub fn normalize_selection(
    view: &RegistryView,
    sel: &ModelSelection,
) -> Option<ModelSelection> {
    let model = view.model(&sel.provider_id, &sel.model_id)?;
    let level = sel.options.as_ref().and_then(|o| o.reasoning_level.as_deref());
    if let Some(level) = level
        && model.config.reasoning_levels.iter().any(|v| v == level)
    {
        return Some(sel.clone());
    }
    Some(ModelSelection {
        provider_id: sel.provider_id.clone(),
        model_id: sel.model_id.clone(),
        options: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_key_cfg(base_url: &str, builtin: &[&str]) -> ProviderConfig {
        ProviderConfig {
            group: "custom".into(),
            access: Access::ApiKey { api_key: Some("sk-test".into()), api_key_management_url: None },
            api: ApiConfig { kind: "openai".into(), base_url: base_url.into(), headers: BTreeMap::new() },
            builtin_model_ids: builtin.iter().map(|s| s.to_string()).collect(),
            personal_model_ids: Vec::new(),
            model_order: Vec::new(),
            visibility: Visibility::Visible,
            enabled: None,
        }
    }

    fn account_cfg(builtin: &[&str]) -> ProviderConfig {
        ProviderConfig {
            group: "account".into(),
            access: Access::Account {
                account_type: "coding-plan".into(),
                mode: "standard".into(),
                entitled: true,
            },
            api: ApiConfig { kind: "openai".into(), base_url: "https://api.example.test".into(), headers: BTreeMap::new() },
            builtin_model_ids: builtin.iter().map(|s| s.to_string()).collect(),
            personal_model_ids: Vec::new(),
            model_order: Vec::new(),
            visibility: Visibility::Visible,
            enabled: None,
        }
    }

    fn model_cfg(levels: &[&str], enabled: bool) -> ModelConfig {
        ModelConfig {
            enabled,
            context_window: 200_000,
            supports_tool_call: true,
            reasoning_levels: levels.iter().map(|s| s.to_string()).collect(),
            max_output_tokens: Some(8192),
        }
    }

    fn catalog(entries: &[(&str, &str, ModelConfig)]) -> ModelCatalog {
        let mut m: BTreeMap<String, BTreeMap<String, ModelConfig>> = BTreeMap::new();
        for (p, mid, cfg) in entries {
            m.entry(p.to_string()).or_default().insert(mid.to_string(), cfg.clone());
        }
        ModelCatalog(m)
    }

    #[test]
    fn owned_order_keeps_unsorted_builtin_first_and_personal_last() {
        let ordered = resolve_owned_order(
            &["b1".into(), "b2".into(), "b3".into()],
            &["p1".into(), "p2".into()],
            &["b2".into(), "p1".into(), "b1".into()],
        );
        // ordered middle preserves the requested order; untouched builtin
        // head and personal tail keep their own relative order
        assert_eq!(ordered, ["b3", "b2", "p1", "b1", "p2"]);
        // duplicates and unknowns are dropped; "x" appears in the requested
        // order so it keeps the user's position (after y), not builtin head
        let dup = resolve_owned_order(
            &["x".into(), "x".into()],
            &["y".into()],
            &["y".into(), "y".into(), "zz".into(), "x".into()],
        );
        assert_eq!(dup, ["y", "x"]);
    }

    #[test]
    fn personal_overlay_replaces_only_set_fields() {
        let base = api_key_cfg("https://builtin.test", &["m1"]);
        let overlay = ProviderOverlay {
            api: Some(ApiConfig { kind: "openai".into(), base_url: "https://personal.test".into(), headers: BTreeMap::new() }),
            ..Default::default()
        };
        let merged = overlay.apply_to(base);
        assert_eq!(merged.api.base_url, "https://personal.test");
        assert_eq!(merged.group, "custom"); // untouched builtin field survives
        assert_eq!(merged.builtin_model_ids, ["m1"]);
    }

    #[test]
    fn resolve_publishes_builtin_then_personal_only_providers() {
        let config = ConfigSnapshot {
            revision: "r1".into(),
            zcode_builtin_revision: "builtin-1".into(),
            zcode_builtin_providers: BTreeMap::from([
                ("zai".to_string(), account_cfg(&["glm"]),
                 ),
                ("api-key-a".to_string(), api_key_cfg("https://a.test", &["m-a"])),
            ]),
            personal_providers: BTreeMap::from([(
                "my-provider".to_string(),
                ProviderOverlay {
                    api: Some(ApiConfig { kind: "openai".into(), base_url: "https://mine.test".into(), headers: BTreeMap::new() }),
                    access: Some(Access::ApiKey { api_key: Some("sk".into()), api_key_management_url: None }),
                    personal_model_ids: Some(vec!["my-model".into()]),
                    ..Default::default()
                },
            )]),
            personal_provider_order: vec!["my-provider".into()],
        };
        let cat = catalog(&[
            ("zai", "glm", model_cfg(&["low", "high"], true)),
            ("api-key-a", "m-a", model_cfg(&["high"], true)),
            ("my-provider", "my-model", model_cfg(&["mid"], true)),
        ]);
        let view = resolve(&config, None, &cat);
        let ids: Vec<&str> = view.providers.iter().map(|p| p.provider_id.as_str()).collect();
        assert_eq!(ids, ["api-key-a", "zai", "my-provider"]);
        assert!(view.provider("my-provider").unwrap().executable);
        assert!(view.model("my-provider", "my-model").unwrap().selectable);
    }

    #[test]
    fn unentitled_account_publishes_no_selectable_models() {
        let config = ConfigSnapshot {
            revision: "r1".into(),
            zcode_builtin_revision: "builtin-1".into(),
            zcode_builtin_providers: BTreeMap::from([("zai".to_string(), account_cfg(&["glm"]))]),
            ..Default::default()
        };
        let cat = catalog(&[("zai", "glm", model_cfg(&["high"], true))]);

        // entitled account: executable + selectable
        let ok = resolve(&config, None, &cat);
        let p = ok.provider("zai").unwrap();
        assert!(p.entitled && p.executable);
        assert!(p.model("glm").unwrap().selectable);

        // fail-closed account: settings entry remains, models do not
        let fc = fail_closed_account(&config);
        let view = resolve(&config, Some(&fc), &cat);
        let p = view.provider("zai").unwrap();
        assert!(!p.entitled);
        assert!(!p.executable);
        let m = p.model("glm").unwrap();
        assert!(!m.executable && !m.selectable);
        assert_eq!(m.config.context_window, 200_000); // settings data survives
    }

    #[test]
    fn fail_closed_masks_every_account_provider() {
        let config = ConfigSnapshot {
            revision: "r1".into(),
            zcode_builtin_revision: "builtin-7".into(),
            zcode_builtin_providers: BTreeMap::from([
                ("zai".to_string(), account_cfg(&["glm"])),
                ("plain".to_string(), api_key_cfg("https://x.test", &["m"])),
            ]),
            ..Default::default()
        };
        let account = fail_closed_account(&config);
        assert!(account.providers.contains_key("zai"));
        assert!(!account.providers.contains_key("plain"));
        assert_eq!(account.based_on_zcode_builtin_revision, "builtin-7");
        assert!(matches!(
            account.providers["zai"].access,
            Some(Access::Account { entitled: false, .. })
        ));
    }

    #[test]
    fn builtin_revision_recovery_trigger() {
        let config_rev = "builtin-9";
        let fresh = AccountSnapshot {
            revision: "a1".into(),
            based_on_zcode_builtin_revision: "builtin-9".into(),
            providers: BTreeMap::new(),
            states: BTreeMap::new(),
        };
        let stale = AccountSnapshot { based_on_zcode_builtin_revision: "builtin-8".into(), ..fresh.clone() };
        assert!(!needs_account_recovery(config_rev, Some(&fresh)));
        assert!(needs_account_recovery(config_rev, Some(&stale)));
        assert!(needs_account_recovery(config_rev, None));
    }

    #[test]
    fn non_current_account_keeps_entry_but_publishes_nothing() {
        let config = ConfigSnapshot {
            revision: "r1".into(),
            zcode_builtin_revision: "b1".into(),
            zcode_builtin_providers: BTreeMap::from([("zai".to_string(), account_cfg(&["glm"]))]),
            ..Default::default()
        };
        let cat = catalog(&[("zai", "glm", model_cfg(&["high"], true))]);
        let account = AccountSnapshot {
            revision: "a1".into(),
            based_on_zcode_builtin_revision: "b1".into(),
            providers: BTreeMap::new(),
            states: BTreeMap::from([("zai".to_string(), AccountState { current: Some(false) })]),
        };
        let view = resolve(&config, Some(&account), &cat);
        let p = view.provider("zai").unwrap();
        assert!(p.entitled); // entitlement is fine — the connection is not current
        assert!(!p.account_current);
        assert!(!p.executable);
        assert!(!p.model("glm").unwrap().selectable);
    }

    #[test]
    fn effective_selection_redirects_account_plan_to_the_current_account() {
        let view = RegistryView {
            revision: 1,
            providers: vec![
                RegistryProvider {
                    provider_id: "zai-work".into(),
                    enabled: true,
                    entitled: true,
                    account_current: true,
                    executable: true,
                    config: account_cfg(&["glm"]),
                    models: vec![RegistryModel {
                        model_id: "glm".into(),
                        source: ModelSource::Builtin,
                        enabled: true,
                        executable: true,
                        selectable: true,
                        config: model_cfg(&["low", "high"], true),
                    }],
                },
            ],
        };
        let states = BTreeMap::from([("zai-work".to_string(), AccountState { current: Some(true) })]);
        let classify = |id: &ProviderId| {
            if id.starts_with("zai") { ProviderKind::AccountPlan } else { ProviderKind::Ordinary }
        };
        let sel = ModelSelection {
            provider_id: "zai-any".into(), // the SAVED plan id, not the connected one
            model_id: "glm".into(),
            options: Some(SelectionOptions { reasoning_level: Some("low".into()) }),
        };
        let eff = resolve_effective(&view, &states, &classify, Some(&sel));
        assert_eq!(eff.issue, None);
        assert_eq!(eff.selection.unwrap().provider_id, "zai-work");

        // zero or two current connections → unavailable, never guessed
        let two = BTreeMap::from([
            ("zai-work".to_string(), AccountState { current: Some(true) }),
            ("zai-home".to_string(), AccountState { current: Some(true) }),
        ]);
        let eff = resolve_effective(&view, &two, &classify, Some(&sel));
        assert_eq!(eff.issue, Some(SelectionIssue::AccountConnectionUnavailable));
        let eff = resolve_effective(&view, &BTreeMap::new(), &classify, Some(&sel));
        assert_eq!(eff.issue, Some(SelectionIssue::AccountConnectionUnavailable));
    }

    #[test]
    fn effective_selection_reports_missing_selection_and_stale_levels() {
        let view = RegistryView { revision: 1, providers: Vec::new() };
        let states = BTreeMap::new();
        let classify = |_: &str| ProviderKind::Ordinary;
        let eff = resolve_effective(&view, &states, &classify, None);
        assert_eq!(eff.issue, Some(SelectionIssue::SelectionMissing));
        assert!(eff.selection.is_none());
    }

    #[test]
    fn hidden_provider_is_unresolvable_except_offpeak() {
        let mut cfg = api_key_cfg("https://h.test", &["m"]);
        cfg.visibility = Visibility::Hidden;
        let view = RegistryView {
            revision: 1,
            providers: vec![RegistryProvider {
                provider_id: "hidden".into(),
                enabled: true,
                entitled: true,
                account_current: true,
                executable: true,
                config: cfg,
                models: vec![RegistryModel {
                    model_id: "m".into(),
                    source: ModelSource::Builtin,
                    enabled: true,
                    executable: true,
                    selectable: false,
                    config: model_cfg(&["high"], true),
                }],
            }],
        };
        let states = BTreeMap::new();
        let sel = ModelSelection {
            provider_id: "hidden".into(),
            model_id: "m".into(),
            options: Some(SelectionOptions { reasoning_level: Some("high".into()) }),
        };
        let eff = resolve_effective(&view, &states, &|_| ProviderKind::Ordinary, Some(&sel));
        assert_eq!(eff.issue, Some(SelectionIssue::ProviderNotFound("hidden".into())));
        let eff = resolve_effective(&view, &states, &|_| ProviderKind::AccountOffpeak, Some(&sel));
        assert!(eff.selection.is_some());
    }

    #[test]
    fn validate_selection_issue_codes() {
        let mut cfg = api_key_cfg("https://x.test", &["m"]);
        cfg.visibility = Visibility::Visible;
        let view = RegistryView {
            revision: 1,
            providers: vec![RegistryProvider {
                provider_id: "p".into(),
                enabled: true,
                entitled: true,
                account_current: true,
                executable: true,
                config: cfg,
                models: vec![RegistryModel {
                    model_id: "m".into(),
                    source: ModelSource::Builtin,
                    enabled: true,
                    executable: true,
                    selectable: true,
                    config: model_cfg(&["low", "high"], true),
                }],
            }],
        };
        assert_eq!(
            validate_selection(&view, &ModelSelection { provider_id: "nope".into(), model_id: "m".into(), options: None }),
            Err(SelectionIssue::ProviderNotFound("nope".into()))
        );
        assert_eq!(
            validate_selection(&view, &ModelSelection { provider_id: "p".into(), model_id: "x".into(), options: None }),
            Err(SelectionIssue::ModelNotFound("p".into(), "x".into()))
        );
        assert_eq!(
            validate_selection(&view, &ModelSelection { provider_id: "p".into(), model_id: "m".into(), options: None }),
            Err(SelectionIssue::ReasoningLevelMissing)
        );
        assert_eq!(
            validate_selection(&view, &ModelSelection {
                provider_id: "p".into(),
                model_id: "m".into(),
                options: Some(SelectionOptions { reasoning_level: Some("ultra".into()) }),
            }),
            Err(SelectionIssue::ReasoningLevelNotSupported("low, high".into()))
        );
        assert!(validate_selection(&view, &ModelSelection {
            provider_id: "p".into(),
            model_id: "m".into(),
            options: Some(SelectionOptions { reasoning_level: Some("high".into()) }),
        })
        .is_ok());
    }

    #[test]
    fn initial_resolution_prefers_selectable_default_then_registry_fallback() {
        let mut first = api_key_cfg("https://1.test", &["m1"]);
        first.group = "first".into();
        let view = RegistryView {
            revision: 3,
            providers: vec![
                RegistryProvider {
                    provider_id: "p1".into(),
                    enabled: true,
                    entitled: true,
                    account_current: true,
                    executable: true,
                    config: first,
                    models: vec![RegistryModel {
                        model_id: "m1".into(),
                        source: ModelSource::Builtin,
                        enabled: true,
                        executable: true,
                        selectable: true,
                        config: model_cfg(&["low", "mid", "high"], true),
                    }],
                },
                RegistryProvider {
                    provider_id: "p2".into(),
                    enabled: true,
                    entitled: true,
                    account_current: true,
                    executable: true,
                    config: api_key_cfg("https://2.test", &["m2"]),
                    models: vec![RegistryModel {
                        model_id: "m2".into(),
                        source: ModelSource::Builtin,
                        enabled: true,
                        executable: true,
                        selectable: true,
                        config: model_cfg(&["high"], true),
                    }],
                },
            ],
        };
        // a selectable configured default wins
        let def = ModelSelection {
            provider_id: "p2".into(),
            model_id: "m2".into(),
            options: Some(SelectionOptions { reasoning_level: Some("high".into()) }),
        };
        assert_eq!(
            resolve_initial(&view, Some(&def)),
            InitialSelection::ConfiguredDefault(def.clone())
        );
        // a stale default (bad level) is discarded → registry fallback,
        // first visible provider, completed to the HIGHEST level
        let stale = ModelSelection { options: Some(SelectionOptions { reasoning_level: Some("gone".into()) }), ..def };
        assert_eq!(
            resolve_initial(&view, Some(&stale)),
            InitialSelection::RegistryFallback(ModelSelection {
                provider_id: "p1".into(),
                model_id: "m1".into(),
                options: Some(SelectionOptions { reasoning_level: Some("high".into()) }),
            })
        );
        // nothing at all
        assert_eq!(resolve_initial(&RegistryView { revision: 0, providers: vec![] }, None), InitialSelection::None);
    }

    #[test]
    fn normalize_keeps_identity_drops_stale_options() {
        let view = RegistryView {
            revision: 1,
            providers: vec![RegistryProvider {
                provider_id: "p".into(),
                enabled: true,
                entitled: true,
                account_current: true,
                executable: true,
                config: api_key_cfg("https://x.test", &["m"]),
                models: vec![RegistryModel {
                    model_id: "m".into(),
                    source: ModelSource::Builtin,
                    enabled: true,
                    executable: true,
                    selectable: true,
                    config: model_cfg(&["low", "high"], true),
                }],
            }],
        };
        let good = ModelSelection {
            provider_id: "p".into(),
            model_id: "m".into(),
            options: Some(SelectionOptions { reasoning_level: Some("low".into()) }),
        };
        assert_eq!(normalize_selection(&view, &good), Some(good.clone()));
        let stale = ModelSelection { options: Some(SelectionOptions { reasoning_level: Some("gone".into()) }), ..good };
        assert_eq!(
            normalize_selection(&view, &stale),
            Some(ModelSelection { provider_id: "p".into(), model_id: "m".into(), options: None })
        );
        assert_eq!(normalize_selection(&view, &ModelSelection { provider_id: "p".into(), model_id: "gone".into(), options: None }), None);
    }

    #[test]
    fn disabled_model_is_listed_but_not_selectable() {
        let config = ConfigSnapshot {
            revision: "r1".into(),
            zcode_builtin_revision: "b1".into(),
            zcode_builtin_providers: BTreeMap::from([(
                "p".to_string(),
                api_key_cfg("https://x.test", &["on", "off"]),
            )]),
            ..Default::default()
        };
        let cat = catalog(&[
            ("p", "on", model_cfg(&["high"], true)),
            ("p", "off", model_cfg(&["high"], false)),
        ]);
        let view = resolve(&config, None, &cat);
        let p = view.provider("p").unwrap();
        assert!(p.model("on").unwrap().selectable);
        let off = p.model("off").unwrap();
        assert!(!off.enabled && !off.executable && !off.selectable);
        // model ordering: builtin ids keep order regardless of catalog order
        let ids: Vec<&str> = p.models.iter().map(|m| m.model_id.as_str()).collect();
        assert_eq!(ids, ["on", "off"]);
    }

    #[test]
    fn model_order_reorders_across_builtin_and_personal() {
        let mut cfg = api_key_cfg("https://x.test", &["b1", "b2"]);
        cfg.personal_model_ids = vec!["p1".into(), "b2".into()]; // b2 dup: personal is filtered
        cfg.model_order = vec!["p1".into(), "b1".into()];
        let config = ConfigSnapshot {
            revision: "r1".into(),
            zcode_builtin_revision: "b1".into(),
            zcode_builtin_providers: BTreeMap::from([("p".to_string(), cfg)]),
            ..Default::default()
        };
        let cat = catalog(&[
            ("p", "b1", model_cfg(&["high"], true)),
            ("p", "b2", model_cfg(&["high"], true)),
            ("p", "p1", model_cfg(&["high"], true)),
        ]);
        let view = resolve(&config, None, &cat);
        let ids: Vec<&str> = view.provider("p").unwrap().models.iter().map(|m| m.model_id.as_str()).collect();
        // unsorted builtin (b2) stays BEFORE the user's requested order;
        // everything requested keeps its own relative order
        assert_eq!(ids, ["b2", "p1", "b1"]);
        assert_eq!(view.provider("p").unwrap().model("p1").unwrap().source, ModelSource::Personal);
    }

    #[test]
    fn snapshots_serialize_with_zcode_wire_shapes() {
        let config = ConfigSnapshot {
            revision: "cfg-1".into(),
            zcode_builtin_revision: "builtin-1".into(),
            zcode_builtin_providers: BTreeMap::from([("zai".to_string(), account_cfg(&["glm"]))]),
            ..Default::default()
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["zcodeBuiltinRevision"], "builtin-1");
        assert_eq!(json["zcodeBuiltinProviders"]["zai"]["access"]["type"], "account");
        assert_eq!(json["zcodeBuiltinProviders"]["zai"]["access"]["entitled"], true);

        let account = fail_closed_account(&config);
        let json = serde_json::to_value(&account).unwrap();
        assert_eq!(json["basedOnZcodeBuiltinRevision"], "builtin-1");
        assert_eq!(json["providers"]["zai"]["access"]["entitled"], false);

        let sel = ModelSelection {
            provider_id: "zai".into(),
            model_id: "glm".into(),
            options: Some(SelectionOptions { reasoning_level: Some("high".into()) }),
        };
        let json = serde_json::to_value(&sel).unwrap();
        assert_eq!(json["providerId"], "zai");
        assert_eq!(json["options"]["reasoningLevel"], "high");

        // round-trip
        let back: ConfigSnapshot = serde_json::from_value(serde_json::to_value(&config).unwrap()).unwrap();
        assert_eq!(back, config);
    }
}
