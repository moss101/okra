//! Official Server MCP authentication (MASTER-PLAN §3 #48, from ZCode
//! `shared/official-mcp-auth.ts` + `services/official-mcp/`): the
//! reserved-header guard, credential resolution for the user's coding
//! plan, request identity headers, and the first-issuance audit.
//!
//! Contracts ported from the TS sources:
//! - **one reserved list, two consumers**: the host produces identity
//!   headers and the plugin adapter strips static ones — both MUST share
//!   the same source, or a static plugin header can shadow credentials.
//!   The legacy `x-coding-plan-api-key` stays blacklisted even though the
//!   client no longer sends it: the server still honors that channel, so
//!   removing it would let a plugin smuggle plan credentials via static
//!   headers;
//! - **family-scoped credentials, no cross-family fallback**: the MaaS
//!   JWT key is `oauth:<family>:access_token`; a ZAI business JWT against
//!   BigModel only yields a doomed request with a misleading failure. The
//!   global login JWT (`zcodejwttoken`) is the mirror of the active
//!   provider — it must match the selected plan's family or the
//!   resolution fails (that check prevents splicing a ZAI JWT and a
//!   BigModel key into one request);
//! - **two-read stability guard**: the registry view, active provider,
//!   both JWTs, and the dynamic account access can all change while
//!   resolving. Everything is read before and after; ANY difference
//!   retries the whole pass (max 2) and then fails `unavailable` — two
//!   generations of credentials are never spliced into one request;
//! - **identity-only degradation**: with a stable identity but no
//!   (exactly one) coding-plan connection, the snapshot carries the bare
//!   JWT with null scopes (`plan_required` is only for malformed team
//!   state; "no plan at all" is a valid identity-only outcome);
//! - **team wire scope is bigmodel-only**: plan scope records the
//!   organization/project either way, but ZAI team connections send no
//!   target headers (matching the Off-Peak behavior);
//! - **team headers are atomic**: organization and project are sent
//!   together or not at all.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model_provider::{Access, RegistryView};

// ---------------------------------------------------------------------------
// shared contract (official-mcp-auth.ts)
// ---------------------------------------------------------------------------

pub const AUTH_HEADER_AUTHORIZATION: &str = "Authorization";
pub const AUTH_HEADER_CODING_PLAN: &str = "X-Bigmodel-Authorization";
pub const AUTH_HEADER_TARGET_TYPE: &str = "Bigmodel-Target-Type";
pub const AUTH_HEADER_ORGANIZATION: &str = "Bigmodel-Organization";
pub const AUTH_HEADER_PROJECT: &str = "Bigmodel-Project";

/// Cross-language protocol key on `params._meta` for stdio official MCP.
/// Renaming breaks every published plugin — a protocol breaking change.
pub const AUTH_META_KEY: &str = "com.zcode/official-mcp-auth";

/// Legacy channel: client no longer sends it, server still honors it —
/// it MUST stay on the blacklist.
pub const LEGACY_API_KEY_HEADER: &str = "x-coding-plan-api-key";

/// The reserved static-header names (lowercase; compare case-insensitively).
pub fn reserved_header_names() -> BTreeSet<String> {
    [
        AUTH_HEADER_AUTHORIZATION,
        AUTH_HEADER_CODING_PLAN,
        AUTH_HEADER_TARGET_TYPE,
        AUTH_HEADER_ORGANIZATION,
        AUTH_HEADER_PROJECT,
        LEGACY_API_KEY_HEADER,
        "mcp-session-id",
        "mcp-protocol-version",
    ]
    .into_iter()
    .map(str::to_ascii_lowercase)
    .collect()
}

/// Case-insensitive reserved check (`isOfficialMcpReservedHeaderName`).
pub fn is_reserved_header_name(name: &str) -> bool {
    reserved_header_names().contains(&name.trim().to_ascii_lowercase())
}

/// Reserved names hit by a static header record, lowercase, deduped,
/// sorted (`findOfficialMcpReservedHeaders`).
pub fn find_reserved_headers(headers: &BTreeMap<String, String>) -> Vec<String> {
    let reserved = reserved_header_names();
    let mut hits = BTreeSet::new();
    for name in headers.keys() {
        let normalized = name.trim().to_ascii_lowercase();
        if reserved.contains(&normalized) {
            hits.insert(normalized);
        }
    }
    hits.into_iter().collect()
}

/// Port-level failure classes ("no request was sent"); network-layer
/// failures (401/403) are classified by the adapter, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthFailureReason {
    #[serde(rename = "official_auth_unavailable")]
    Unavailable,
    #[serde(rename = "official_auth_plan_required")]
    PlanRequired,
}

/// `Bigmodel-Target-Type` values (aligned with zcode-server).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TargetType {
    Personal,
    Team,
}

// ---------------------------------------------------------------------------
// plan scope + credential snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "targetType", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PlanScope {
    Personal,
    Team {
        #[serde(rename = "organizationId")]
        organization_id: String,
        #[serde(rename = "projectId")]
        project_id: String,
    },
}

impl PlanScope {
    pub fn target_type(&self) -> TargetType {
        match self {
            PlanScope::Personal => TargetType::Personal,
            PlanScope::Team { .. } => TargetType::Team,
        }
    }
}

/// Provider family; the credential keys are family-scoped on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderFamily {
    Zai,
    Bigmodel,
}

impl ProviderFamily {
    fn oauth_provider_id(self) -> &'static str {
        match self {
            ProviderFamily::Zai => "zai",
            ProviderFamily::Bigmodel => "bigmodel",
        }
    }

    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "zai" => Some(ProviderFamily::Zai),
            "bigmodel" => Some(ProviderFamily::Bigmodel),
            _ => None,
        }
    }
}

/// Resolved credential snapshot — lives in host memory only; it must be
/// redacted before crossing any RPC boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialSnapshot {
    /// Global login JWT (no Bearer prefix; headers add it).
    pub jwt: String,
    /// MaaS login JWT for the plan channel (`oauth:<family>:access_token`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coding_plan_authorization: Option<String>,
    pub provider_family: ProviderFamily,
    /// Ownership of the current connection; null when unattributable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_scope: Option<PlanScope>,
    /// Scope actually SENT to the server MCP; ZAI team stays null.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_scope: Option<PlanScope>,
}

// ---------------------------------------------------------------------------
// resolution seams (sync ports of the async deps)
// ---------------------------------------------------------------------------

pub trait CredentialSource {
    fn load(&self, key: &str) -> Option<String>;
}

/// Resolves the CURRENT dynamic account access for a provider's static
/// account access (`accountRequestAuthService.resolveAccessCurrent`).
pub trait AccountAccessResolver {
    fn resolve_access_current(&mut self, access: &ProviderAccountAccess) -> Option<AccountAccess>;
}

/// Static access on a provider entry (registry view projection).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAccountAccess {
    pub account_type: String,
    /// `individual-coding-plan` | `team-coding-plan` are the only modes
    /// that count as coding-plan providers.
    pub mode: String,
    #[serde(default)]
    pub entitled: bool,
}

/// Dynamic account access (`ZCodeAccountAccess`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountAccess {
    pub family: String,
    pub plan_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
}

/// Fresh view of the model-selection registry; called TWICE per pass so
/// tests can inject rotation between the reads.
pub trait SelectionViewSource {
    fn view(&self) -> RegistryView;
}

const ACTIVE_OAUTH_PROVIDER_KEY: &str = "oauth:active_provider";
const ZCODE_JWT_TOKEN_KEY: &str = "zcodejwttoken";

/// Provider-entry gate only: exactly ONE account access in a coding-plan
/// mode may be present (`resolveSelectedProvider`). Static configs can
/// never fabricate a team scope — that comes from the account service.
fn select_coding_plan_provider(
    view: &RegistryView,
) -> Result<(String, ProviderAccountAccess), AuthFailureReason> {
    let mut candidates = Vec::new();
    for p in &view.providers {
        if let Access::Account { account_type, mode, entitled } = &p.config.access
            && (mode == "individual-coding-plan" || mode == "team-coding-plan")
        {
            candidates.push((
                p.provider_id.clone(),
                ProviderAccountAccess {
                    account_type: account_type.clone(),
                    mode: mode.clone(),
                    entitled: *entitled,
                },
            ));
        }
    }
    if candidates.len() != 1 {
        return Err(AuthFailureReason::PlanRequired);
    }
    Ok(candidates.swap_remove(0))
}

/// Non-secret selection fingerprint: proves both reads of a pass saw the
/// same registry generation (no pre/post splice).
fn selection_fingerprint(view: &RegistryView) -> String {
    let json = serde_json::to_string(view).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(json.as_bytes());
    format!("{:x}", hasher.finalize())
}

struct IdentitySnapshot {
    active_family: ProviderFamily,
    jwt: String,
    fingerprint: String,
}

fn read_identity_snapshot(
    view_source: &dyn SelectionViewSource,
    credentials: &dyn CredentialSource,
) -> Result<IdentitySnapshot, AuthFailureReason> {
    let view = view_source.view();
    let active = credentials
        .load(ACTIVE_OAUTH_PROVIDER_KEY)
        .map(|v| v.trim().to_string());
    let jwt = credentials
        .load(ZCODE_JWT_TOKEN_KEY)
        .map(|v| v.trim().to_string())
        .unwrap_or_default();
    let Some(active) = active else {
        return Err(AuthFailureReason::Unavailable);
    };
    let Some(active_family) = ProviderFamily::from_wire(&active) else {
        return Err(AuthFailureReason::Unavailable);
    };
    if jwt.is_empty() {
        return Err(AuthFailureReason::Unavailable);
    }
    Ok(IdentitySnapshot {
        active_family,
        jwt,
        fingerprint: selection_fingerprint(&view),
    })
}

/// `maasJwtCredentialKey`: family-exact, NEVER cross-family fallback.
fn maas_jwt_credential_key(family: ProviderFamily) -> String {
    format!("oauth:{}:access_token", family.oauth_provider_id())
}

struct SelectedPlan {
    family: ProviderFamily,
    plan_scope: Option<PlanScope>,
    wire_scope: Option<PlanScope>,
}

/// Dynamic plan resolution (`resolveSelectedPlan`): family and plan kind
/// must agree with the static provider access; team scope comes only
/// from the account service.
fn resolve_selected_plan(
    access: &ProviderAccountAccess,
    dynamic: Option<&AccountAccess>,
) -> Result<SelectedPlan, AuthFailureReason> {
    let Some(dynamic) = dynamic else {
        return Err(AuthFailureReason::PlanRequired);
    };
    let Some(family) = ProviderFamily::from_wire(&dynamic.family) else {
        return Err(AuthFailureReason::PlanRequired);
    };
    if family != ProviderFamily::from_wire(&access.account_type).unwrap_or(family)
        || dynamic.family != access.account_type
    {
        return Err(AuthFailureReason::PlanRequired);
    }
    let team = access.mode == "team-coding-plan";
    if team != (dynamic.plan_kind == "team-coding-plan") {
        return Err(AuthFailureReason::PlanRequired);
    }
    if team {
        let (Some(org), Some(project)) = (&dynamic.organization_id, &dynamic.project_id) else {
            return Err(AuthFailureReason::PlanRequired);
        };
        let team_scope = PlanScope::Team {
            organization_id: org.clone(),
            project_id: project.clone(),
        };
        let wire_scope = (family == ProviderFamily::Bigmodel).then(|| team_scope.clone());
        Ok(SelectedPlan {
            family,
            plan_scope: Some(team_scope),
            wire_scope,
        })
    } else {
        Ok(SelectedPlan {
            family,
            plan_scope: Some(PlanScope::Personal),
            wire_scope: Some(PlanScope::Personal),
        })
    }
}

/// Deps bundle for one resolution pass.
pub struct ResolverDeps<'a> {
    pub view_source: &'a dyn SelectionViewSource,
    pub credentials: &'a dyn CredentialSource,
    pub accounts: &'a mut dyn AccountAccessResolver,
}

/// Resolve the official MCP credentials for the current connection
/// (`resolveOfficialMcpCredentials`): at most two passes; every input is
/// re-read at the end of a pass and ANY instability retries — splicing
/// two credential generations into one request is the one unrecoverable
/// outcome this guard exists to prevent.
pub fn resolve_official_mcp_credentials(
    deps: &mut ResolverDeps<'_>,
) -> Result<CredentialSnapshot, AuthFailureReason> {
    for attempt in 0..2 {
        let Ok(identity) = read_identity_snapshot(deps.view_source, deps.credentials) else {
            if attempt == 0 {
                continue; // identity appeared mid-flight; one retry
            }
            return Err(AuthFailureReason::Unavailable);
        };

        let view = deps.view_source.view();
        let provider = match select_coding_plan_provider(&view) {
            Ok(found) => found,
            // no plan provider: degrade to identity-only, but only if the
            // identity is STABLE (otherwise fall through to a retry)
            Err(reason) => {
                let latest = read_identity_snapshot(deps.view_source, deps.credentials);
                match latest {
                    Ok(latest) if latest.fingerprint == identity.fingerprint => {
                        return Ok(CredentialSnapshot {
                            jwt: identity.jwt,
                            coding_plan_authorization: None,
                            provider_family: identity.active_family,
                            plan_scope: None,
                            wire_scope: None,
                        });
                    }
                    _ => {
                        let _ = reason;
                        continue;
                    }
                }
            }
        };

        let Some(access) = deps.accounts.resolve_access_current(&provider.1) else {
            return Err(AuthFailureReason::PlanRequired);
        };
        let plan = resolve_selected_plan(&provider.1, Some(&access))?;

        // the global login JWT mirrors the active provider — a mismatch
        // means the plan belongs to the OTHER family: fail, never splice
        if identity.active_family != plan.family {
            return Err(AuthFailureReason::Unavailable);
        }

        let maas_key = maas_jwt_credential_key(plan.family);
        let maas_jwt = deps
            .credentials
            .load(&maas_key)
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if maas_jwt.is_empty() {
            // login state is incomplete (re-login needed), NOT plan_required
            return Err(AuthFailureReason::Unavailable);
        }

        // stability gate: identity + selection + dynamic access + JWT
        let latest_identity = read_identity_snapshot(deps.view_source, deps.credentials);
        let latest_view = deps.view_source.view();
        let latest_provider = select_coding_plan_provider(&latest_view);
        let latest_access = match &latest_provider {
            Ok(latest_provider) => deps.accounts.resolve_access_current(&latest_provider.1),
            Err(_) => None,
        };
        let latest_maas = deps
            .credentials
            .load(&maas_key)
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        let identity_stable = matches!(&latest_identity, Ok(latest) if latest.fingerprint == identity.fingerprint && latest.jwt == identity.jwt && latest.active_family == identity.active_family);
        let access_stable = serde_json::to_string(&access).ok()
            == latest_access.as_ref().and_then(|a| serde_json::to_string(a).ok());
        if !identity_stable
            || !access_stable
            || latest_provider.as_ref().ok().map(|p| &p.0) != Some(&provider.0)
            || latest_maas != maas_jwt
        {
            continue;
        }

        // the provider entry is the "connection exists" gate only; its
        // business key is no longer a credential (MaaS JWT channel)
        if view.provider(&provider.0).is_none() {
            return Err(AuthFailureReason::PlanRequired);
        }

        return Ok(CredentialSnapshot {
            jwt: identity.jwt,
            coding_plan_authorization: Some(maas_jwt),
            provider_family: plan.family,
            plan_scope: plan.plan_scope,
            wire_scope: plan.wire_scope,
        });
    }
    Err(AuthFailureReason::Unavailable)
}

/// Build this request's identity headers (`buildOfficialMcpAuthHeaders`).
/// Team identity is ATOMIC: organization and project go together or not
/// at all.
pub fn build_official_mcp_auth_headers(
    snapshot: &CredentialSnapshot,
) -> BTreeMap<String, String> {
    let mut headers = BTreeMap::new();
    headers.insert(
        AUTH_HEADER_AUTHORIZATION.to_string(),
        format!("Bearer {}", snapshot.jwt),
    );
    if let Some(plan_jwt) = &snapshot.coding_plan_authorization {
        headers.insert(
            AUTH_HEADER_CODING_PLAN.to_string(),
            format!("Bearer {plan_jwt}"),
        );
    }
    if let Some(scope) = &snapshot.wire_scope {
        headers.insert(
            AUTH_HEADER_TARGET_TYPE.to_string(),
            match scope.target_type() {
                TargetType::Personal => "PERSONAL".to_string(),
                TargetType::Team => "TEAM".to_string(),
            },
        );
        if let PlanScope::Team { organization_id, project_id } = scope {
            headers.insert(AUTH_HEADER_ORGANIZATION.to_string(), organization_id.clone());
            headers.insert(AUTH_HEADER_PROJECT.to_string(), project_id.clone());
        }
    }
    headers
}

// ---------------------------------------------------------------------------
// first-issuance audit (officialMcpIssuanceAudit.ts)
// ---------------------------------------------------------------------------

/// Bounded first-issuance audit: `mark_first` returns true exactly once
/// per (plugin, mcp key, workspace) triple; the oldest entry is evicted
/// when the capacity is exceeded.
pub struct IssuanceAudit {
    capacity: usize,
    order: VecDeque<String>,
    seen: BTreeSet<String>,
}

impl IssuanceAudit {
    pub fn new(max_entries: usize) -> Self {
        Self {
            capacity: max_entries.max(1),
            order: VecDeque::new(),
            seen: BTreeSet::new(),
        }
    }

    pub fn mark_first(&mut self, plugin_id: &str, mcp_key: &str, workspace_key: &str) -> bool {
        let key = format!("{plugin_id}\u{0}{mcp_key}\u{0}{workspace_key}");
        if !self.seen.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        while self.seen.len() > self.capacity {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.seen.remove(&oldest);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_provider::{
        ApiConfig, ModelCatalog, ProviderConfig, RegistryProvider, RegistryView,
    };
    use std::cell::RefCell;

    // ---- test seams -------------------------------------------------------

    struct StaticCredentials(BTreeMap<String, String>);
    impl CredentialSource for StaticCredentials {
        fn load(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    /// Alternates its credential map on EVERY load, simulating a token
    /// that keeps rotating between the reads of every pass. Identity
    /// fields are identical in both maps; the MaaS JWT differs.
    struct RotatingCredentials {
        first: BTreeMap<String, String>,
        second: BTreeMap<String, String>,
        loads: std::cell::Cell<usize>,
    }
    impl CredentialSource for RotatingCredentials {
        fn load(&self, key: &str) -> Option<String> {
            let n = self.loads.get();
            self.loads.set(n + 1);
            let map = if n % 2 == 0 { &self.first } else { &self.second };
            map.get(key).cloned()
        }
    }

    struct StaticAccounts(Option<AccountAccess>);
    impl AccountAccessResolver for StaticAccounts {
        fn resolve_access_current(&mut self, _access: &ProviderAccountAccess) -> Option<AccountAccess> {
            self.0.clone()
        }
    }

    struct StaticView(RegistryView);
    impl SelectionViewSource for StaticView {
        fn view(&self) -> RegistryView {
            self.0.clone()
        }
    }

    fn coding_plan_view(mode: &str, count: usize) -> RegistryView {
        let mut providers = Vec::new();
        let modes = ["individual-coding-plan", "team-coding-plan"];
        for i in 0..count {
            providers.push(RegistryProvider {
                provider_id: format!("cp-{i}"),
                enabled: true,
                entitled: true,
                account_current: true,
                executable: true,
                config: ProviderConfig {
                    group: "account".into(),
                    access: Access::Account {
                        account_type: "bigmodel".into(),
                        mode: if count == 1 { mode.to_string() } else { modes[i % 2].to_string() },
                        entitled: true,
                    },
                    api: ApiConfig {
                        kind: "openai".into(),
                        base_url: "https://api.example.test".into(),
                        headers: BTreeMap::new(),
                    },
                    builtin_model_ids: Vec::new(),
                    personal_model_ids: Vec::new(),
                    model_order: Vec::new(),
                    visibility: crate::model_provider::Visibility::Visible,
                    enabled: None,
                },
                models: Vec::new(),
            });
        }
        RegistryView { revision: 1, providers }
    }

    fn creds(active: &str, jwt: &str, maas: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            (ACTIVE_OAUTH_PROVIDER_KEY.to_string(), active.to_string()),
            (ZCODE_JWT_TOKEN_KEY.to_string(), jwt.to_string()),
            ("oauth:bigmodel:access_token".to_string(), maas.to_string()),
            ("oauth:zai:access_token".to_string(), maas.to_string()),
        ])
    }

    fn individual_access() -> AccountAccess {
        AccountAccess {
            family: "bigmodel".into(),
            plan_kind: "individual-coding-plan".into(),
            organization_id: None,
            project_id: None,
        }
    }

    fn team_access() -> AccountAccess {
        AccountAccess {
            family: "bigmodel".into(),
            plan_kind: "team-coding-plan".into(),
            organization_id: Some("org-1".into()),
            project_id: Some("prj-7".into()),
        }
    }

    fn resolve_now(
        view: RegistryView,
        credentials: &dyn CredentialSource,
        accounts: &mut dyn AccountAccessResolver,
    ) -> Result<CredentialSnapshot, AuthFailureReason> {
        let mut deps = ResolverDeps {
            view_source: &StaticView(view),
            credentials,
            accounts,
        };
        resolve_official_mcp_credentials(&mut deps)
    }

    // ---- reserved headers --------------------------------------------------

    #[test]
    fn reserved_headers_include_legacy_and_protocol_names() {
        let reserved = reserved_header_names();
        assert!(reserved.contains("authorization"));
        assert!(reserved.contains("x-bigmodel-authorization"));
        assert!(reserved.contains("bigmodel-target-type"));
        assert!(reserved.contains("bigmodel-organization"));
        assert!(reserved.contains("bigmodel-project"));
        // legacy channel still blacklisted even though the client never
        // sends it — the server still honors it
        assert!(reserved.contains(LEGACY_API_KEY_HEADER));
        assert!(reserved.contains("mcp-session-id"));
        assert!(reserved.contains("mcp-protocol-version"));
    }

    #[test]
    fn reserved_header_check_is_case_insensitive_and_trims() {
        assert!(is_reserved_header_name("  AUTHORIZATION "));
        assert!(is_reserved_header_name("X-Coding-Plan-Api-Key"));
        assert!(is_reserved_header_name("MCP-Protocol-Version"));
        assert!(!is_reserved_header_name("x-my-own-header"));
    }

    #[test]
    fn find_reserved_headers_returns_sorted_unique_lowercase_hits() {
        let headers = BTreeMap::from([
            ("Session-Id".to_string(), "x".into()),
            ("mcp-session-id".to_string(), "y".into()),
            ("Authorization".to_string(), "z".into()),
            ("my-header".to_string(), "w".into()),
        ]);
        assert_eq!(
            find_reserved_headers(&headers),
            ["authorization", "mcp-session-id"]
        );
        assert!(find_reserved_headers(&BTreeMap::new()).is_empty());
    }

    // ---- issuance audit -----------------------------------------------------

    #[test]
    fn issuance_audit_marks_first_only_and_evicts_oldest() {
        let mut audit = IssuanceAudit::new(2);
        assert!(audit.mark_first("p", "m", "w1"));
        assert!(!audit.mark_first("p", "m", "w1"));
        assert!(audit.mark_first("p", "m", "w2"));
        assert!(audit.mark_first("p", "m", "w3")); // evicts w1
        assert!(audit.mark_first("p", "m", "w1")); // first again
        // capacity floors at 1
        let mut tiny = IssuanceAudit::new(0);
        assert!(tiny.mark_first("a", "b", "c"));
        assert!(!tiny.mark_first("a", "b", "c"));
        assert!(tiny.mark_first("x", "y", "z"));
        assert!(tiny.mark_first("a", "b", "c"));
    }

    // ---- credential resolution ----------------------------------------------

    #[test]
    fn bigmodel_individual_resolves_with_personal_scope_and_headers() {
        let view = coding_plan_view("individual-coding-plan", 1);
        let credentials = StaticCredentials(creds("bigmodel", "global-jwt", "maas-jwt"));
        let mut accounts = StaticAccounts(Some(individual_access()));
        let snap = resolve_now(view, &credentials, &mut accounts).unwrap();
        assert_eq!(snap.provider_family, ProviderFamily::Bigmodel);
        assert_eq!(snap.jwt, "global-jwt");
        assert_eq!(snap.coding_plan_authorization.as_deref(), Some("maas-jwt"));
        assert_eq!(snap.plan_scope, Some(PlanScope::Personal));
        assert_eq!(snap.wire_scope, Some(PlanScope::Personal));
        let headers = build_official_mcp_auth_headers(&snap);
        assert_eq!(headers.get(AUTH_HEADER_AUTHORIZATION).unwrap(), "Bearer global-jwt");
        assert_eq!(headers.get(AUTH_HEADER_CODING_PLAN).unwrap(), "Bearer maas-jwt");
        assert_eq!(headers.get(AUTH_HEADER_TARGET_TYPE).unwrap(), "PERSONAL");
        assert!(!headers.contains_key(AUTH_HEADER_ORGANIZATION));
    }

    #[test]
    fn bigmodel_team_sends_atomic_org_project_headers() {
        let view = coding_plan_view("team-coding-plan", 1);
        let credentials = StaticCredentials(creds("bigmodel", "g", "m"));
        let mut accounts = StaticAccounts(Some(team_access()));
        let snap = resolve_now(view, &credentials, &mut accounts).unwrap();
        assert_eq!(
            snap.plan_scope,
            Some(PlanScope::Team {
                organization_id: "org-1".into(),
                project_id: "prj-7".into()
            })
        );
        let headers = build_official_mcp_auth_headers(&snap);
        assert_eq!(headers.get(AUTH_HEADER_TARGET_TYPE).unwrap(), "TEAM");
        assert_eq!(headers.get(AUTH_HEADER_ORGANIZATION).unwrap(), "org-1");
        assert_eq!(headers.get(AUTH_HEADER_PROJECT).unwrap(), "prj-7");
    }

    #[test]
    fn zai_team_records_plan_scope_but_sends_no_wire_scope() {
        let view = coding_plan_view("team-coding-plan", 1);
        // flip the provider family to zai
        let mut view = view;
        if let Access::Account { account_type, .. } = &mut view.providers[0].config.access {
            *account_type = "zai".into();
        }
        let credentials = StaticCredentials(creds("zai", "g", "m"));
        let mut accounts = StaticAccounts(Some(AccountAccess {
            family: "zai".into(),
            plan_kind: "team-coding-plan".into(),
            organization_id: Some("org-2".into()),
            project_id: Some("prj-8".into()),
        }));
        let snap = resolve_now(view, &credentials, &mut accounts).unwrap();
        assert_eq!(snap.provider_family, ProviderFamily::Zai);
        assert!(matches!(snap.plan_scope, Some(PlanScope::Team { .. })));
        assert_eq!(snap.wire_scope, None, "ZAI team wire scope stays null");
        let headers = build_official_mcp_auth_headers(&snap);
        assert!(!headers.contains_key(AUTH_HEADER_TARGET_TYPE));
        assert!(headers.contains_key(AUTH_HEADER_AUTHORIZATION));
    }

    #[test]
    fn stable_identity_without_plan_degrades_to_identity_only() {
        let view = RegistryView { revision: 1, providers: Vec::new() };
        let credentials = StaticCredentials(creds("bigmodel", "g", "m"));
        let mut accounts = StaticAccounts(None);
        let snap = resolve_now(view, &credentials, &mut accounts).unwrap();
        assert_eq!(snap.jwt, "g");
        assert_eq!(snap.coding_plan_authorization, None);
        assert_eq!(snap.plan_scope, None);
        assert_eq!(snap.wire_scope, None);
        let headers = build_official_mcp_auth_headers(&snap);
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn two_coding_plan_providers_still_degrade_to_identity_only() {
        let view = coding_plan_view("individual-coding-plan", 2);
        let credentials = StaticCredentials(creds("bigmodel", "g", "m"));
        let mut accounts = StaticAccounts(Some(individual_access()));
        let snap = resolve_now(view, &credentials, &mut accounts).unwrap();
        assert_eq!(snap.plan_scope, None);
    }

    #[test]
    fn missing_identity_fails_unavailable() {
        let view = coding_plan_view("individual-coding-plan", 1);
        // no active provider
        let c1 = StaticCredentials(BTreeMap::from([
            (ZCODE_JWT_TOKEN_KEY.to_string(), "g".into()),
            ("oauth:bigmodel:access_token".to_string(), "m".into()),
        ]));
        // empty jwt
        let c2 = StaticCredentials(creds("bigmodel", "  ", "m"));
        let mut accounts = StaticAccounts(Some(individual_access()));
        assert_eq!(
            resolve_now(view.clone(), &c1, &mut accounts),
            Err(AuthFailureReason::Unavailable)
        );
        assert_eq!(
            resolve_now(view, &c2, &mut accounts),
            Err(AuthFailureReason::Unavailable)
        );
    }

    #[test]
    fn cross_family_identity_and_plan_never_splice() {
        let view = coding_plan_view("individual-coding-plan", 1); // bigmodel family
        let credentials = StaticCredentials(creds("zai", "zai-jwt", "maas-jwt"));
        let mut accounts = StaticAccounts(Some(individual_access())); // bigmodel plan
        assert_eq!(
            resolve_now(view, &credentials, &mut accounts),
            Err(AuthFailureReason::Unavailable)
        );
    }

    #[test]
    fn missing_maas_jwt_is_unavailable_not_plan_required() {
        let view = coding_plan_view("individual-coding-plan", 1);
        let mut map = creds("bigmodel", "g", "");
        map.remove("oauth:bigmodel:access_token");
        let credentials = StaticCredentials(map);
        let mut accounts = StaticAccounts(Some(individual_access()));
        assert_eq!(
            resolve_now(view, &credentials, &mut accounts),
            Err(AuthFailureReason::Unavailable)
        );
    }

    #[test]
    fn plan_kind_mismatch_is_plan_required() {
        let view = coding_plan_view("team-coding-plan", 1);
        let credentials = StaticCredentials(creds("bigmodel", "g", "m"));
        let mut accounts = StaticAccounts(Some(individual_access())); // wrong kind
        assert_eq!(
            resolve_now(view, &credentials, &mut accounts),
            Err(AuthFailureReason::PlanRequired)
        );
    }

    #[test]
    fn credential_rotation_between_reads_retries_then_fails_clean() {
        let view = coding_plan_view("individual-coding-plan", 1);
        // the MaaS JWT alternates between the two reads of EVERY pass, so
        // both passes end unstable and the result must be a clean failure —
        // never a snapshot stitched from two generations
        let credentials = RotatingCredentials {
            first: creds("bigmodel", "g1", "maas-v1"),
            second: creds("bigmodel", "g1", "maas-v2"),
            loads: std::cell::Cell::new(0),
        };
        let mut accounts = StaticAccounts(Some(individual_access()));
        assert_eq!(
            resolve_now(view, &credentials, &mut accounts),
            Err(AuthFailureReason::Unavailable)
        );
    }

    #[test]
    fn wire_shapes_serialize_with_zcode_fields() {
        let snap = CredentialSnapshot {
            jwt: "g".into(),
            coding_plan_authorization: Some("m".into()),
            provider_family: ProviderFamily::Bigmodel,
            plan_scope: Some(PlanScope::Team {
                organization_id: "o".into(),
                project_id: "p".into(),
            }),
            wire_scope: Some(PlanScope::Personal),
        };
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(json["providerFamily"], "bigmodel");
        assert_eq!(json["codingPlanAuthorization"], "m");
        assert_eq!(json["planScope"]["targetType"], "TEAM");
        assert_eq!(json["planScope"]["organizationId"], "o");
        assert_eq!(json["wireScope"]["targetType"], "PERSONAL");

        let reason: AuthFailureReason = serde_json::from_str("\"official_auth_plan_required\"").unwrap();
        assert_eq!(reason, AuthFailureReason::PlanRequired);
        assert_eq!(
            serde_json::to_value(AuthFailureReason::Unavailable).unwrap(),
            "official_auth_unavailable"
        );
        // the meta key is a cross-language protocol constant
        assert_eq!(AUTH_META_KEY, "com.zcode/official-mcp-auth");
    }
}
