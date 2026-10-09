pub use crate::tools::tool_groups::{RoleToolConfig, ToolGroupSettings};
use anyhow::Result;
use config::builder::DefaultState;
use config::{Config, ConfigBuilder, ConfigError, Environment, Value, ValueKind};
use serde::Deserialize;
use std::path::Path;
use std::sync::Once;

/// Scalar settings accepted from the AGENT_OS_ environment.
pub(crate) const ENV_KEY_MAP: &[(&str, &str)] = &[
    ("AGENT_OS_API_ENABLE_METRICS", "api.enable_metrics"),
    ("AGENT_OS_API_GRPC_ADDR", "api.grpc_addr"),
    ("AGENT_OS_API_HTTP_ADDR", "api.http_addr"),
    ("AGENT_OS_API_METRICS_PORT", "api.metrics_port"),
    ("AGENT_OS_EMBEDDING_ENABLED", "embedding.enabled"),
    (
        "AGENT_OS_EMBEDDING_FALLBACK_DIMENSION",
        "embedding.fallback.dimension",
    ),
    (
        "AGENT_OS_EMBEDDING_OLLAMA_BASE_URL",
        "embedding.ollama.base_url",
    ),
    (
        "AGENT_OS_EMBEDDING_OLLAMA_DIMENSION",
        "embedding.ollama.dimension",
    ),
    ("AGENT_OS_EMBEDDING_OLLAMA_MODEL", "embedding.ollama.model"),
    (
        "AGENT_OS_EMBEDDING_ONEAPI_API_KEY",
        "embedding.oneapi.api_key",
    ),
    (
        "AGENT_OS_EMBEDDING_ONEAPI_BASE_URL",
        "embedding.oneapi.base_url",
    ),
    (
        "AGENT_OS_EMBEDDING_ONEAPI_DIMENSION",
        "embedding.oneapi.dimension",
    ),
    ("AGENT_OS_EMBEDDING_ONEAPI_MODEL", "embedding.oneapi.model"),
    ("AGENT_OS_EMBEDDING_PROVIDER", "embedding.provider"),
    ("AGENT_OS_GATEWAY_API_KEY", "gateway.api_key"),
    ("AGENT_OS_GATEWAY_BASE_URL", "gateway.base_url"),
    ("AGENT_OS_GATEWAY_DEFAULT_MODEL", "gateway.default_model"),
    ("AGENT_OS_GATEWAY_MAX_RETRIES", "gateway.max_retries"),
    ("AGENT_OS_GATEWAY_RETRY_BASE_MS", "gateway.retry_base_ms"),
    (
        "AGENT_OS_GATEWAY_TIMEOUT_SECONDS",
        "gateway.timeout_seconds",
    ),
    (
        "AGENT_OS_GATEWAY_USE_RESPONSES_API",
        "gateway.use_responses_api",
    ),
    ("AGENT_OS_OUTPUT_DIRECTORY", "output.directory"),
];

/// Process controls read directly by the application, not Settings fields.
const PROCESS_ENV_VARS: &[&str] = &[
    "AGENT_OS_ALLOW_DEFAULT_CONFIG",
    "AGENT_OS_APPROVAL_ENABLED",
    "AGENT_OS_APPROVAL_TIMEOUT",
    "AGENT_OS_CONCURRENCY",
    "AGENT_OS_CONFIG_PROFILE",
    "AGENT_OS_HTTP_PORT",
    "AGENT_OS_L0_PATH",
    "AGENT_OS_L1_MEMORY_MB",
    "AGENT_OS_L2_MEMORY_MB",
    "AGENT_OS_L3_MEMORY_MB",
    "AGENT_OS_QUEUE_PATH",
    "AGENT_OS_WORKSPACE_ROOT",
];

/// `Value` origin tag for settings injected from [`ENV_KEY_MAP`]; followed by
/// the variable name. [`deserialize_with_field_path`] uses it to keep the raw
/// value out of load errors (a mistyped key must not land in startup logs).
const MAPPED_ENV_ORIGIN_PREFIX: &str = "environment variable ";

static WARN_UNKNOWN_ENV: Once = Once::new();

fn is_mapped_or_process_var(name: &str) -> bool {
    ENV_KEY_MAP.iter().any(|(env, _)| *env == name) || PROCESS_ENV_VARS.contains(&name)
}

fn warn_unrecognized_env_vars(env: &[(String, String)]) {
    let mut names: Vec<&str> = env
        .iter()
        .map(|(name, _)| name.as_str())
        .filter(|name| name.starts_with("AGENT_OS_") && !is_mapped_or_process_var(name))
        .collect();
    names.sort_unstable();
    names.dedup();
    if !names.is_empty() {
        tracing::warn!(
            "AGENT_OS_ variables not in the explicit mapping table: {}; they only take effect via legacy `_` splitting, which works only for single-word field names",
            names.join(", ")
        );
    }
}

/// File order is yaml < runtime override < environment. Explicit overrides
/// must be applied last because config's set_override beats every file source.
///
/// Each file is read exactly once per load: the yaml and the runtime override
/// are built into their own [`Config`] first and those same objects are added
/// to the main builder (`Config` is a `Source`), so the endpoint/key binding
/// below sees exactly the values the merged configuration uses (#303 review).
fn config_builder_with_sources(
    yaml_name: &str,
    override_path: &Path,
    env: &[(String, String)],
) -> Result<ConfigBuilder<DefaultState>, ConfigError> {
    let deployment = Config::builder()
        .add_source(config::File::with_name(yaml_name).required(false))
        .build()?;
    let runtime_override = RuntimeOverride::read(override_path)?;
    builder_from_layers(&deployment, runtime_override.as_ref(), env)
}

/// Assemble the main builder from layers that were each read once.
fn builder_from_layers(
    deployment: &Config,
    runtime_override: Option<&RuntimeOverride>,
    env: &[(String, String)],
) -> Result<ConfigBuilder<DefaultState>, ConfigError> {
    let legacy = env
        .iter()
        .filter(|(name, _)| name.starts_with("AGENT_OS_") && !is_mapped_or_process_var(name))
        .cloned()
        .collect();
    // Legacy splitting is a fallback for existing single-word deployments;
    // fields containing `_` must use the explicit table instead.
    let mut builder = Config::builder().add_source(deployment.clone());
    if let Some(runtime_override) = runtime_override {
        builder = builder.add_source(runtime_override.config.clone());
    }
    builder = builder.add_source(
        Environment::with_prefix("AGENT_OS")
            .separator("_")
            .try_parsing(true)
            .source(Some(legacy)),
    );
    for (name, key) in ENV_KEY_MAP {
        if let Some((_, value)) = env.iter().find(|(candidate, _)| candidate == name) {
            // Keep the raw string. `config` coerces strings to bool/integer
            // when the target field asks for one (`Value::into_bool` /
            // `into_uint`), while string fields such as keys, URLs and model
            // names must reach serde verbatim (e.g. `007` or `1e5` must not
            // be reparsed as numbers).
            let kind = ValueKind::String(value.clone());
            let origin = format!("{MAPPED_ENV_ORIGIN_PREFIX}{name}");
            builder = builder.set_override(key, Value::new(Some(&origin), kind))?;
        }
    }
    bind_deployment_keys_to_their_endpoints(builder, deployment, runtime_override, env)
}

/// Top-level sections of the runtime override that hold an endpoint/key pair.
const KEYED_OVERRIDE_SECTIONS: &[&str] = &["gateway", "embedding"];

/// `gateway.model_mapping` maps model names (which may contain upper case) to
/// model names; it can never fold into an endpoint or key field.
const FREE_FORM_KEY_TABLES: &[(&str, &str)] = &[("gateway", "model_mapping")];

/// `config_override.json`, read once per load.
struct RuntimeOverride {
    /// The parsed override, added as-is to the main builder.
    config: Config,
    /// Sections in [`KEYED_OVERRIDE_SECTIONS`] whose raw spelling is not
    /// canonical (see [`noncanonical_keyed_sections`]).
    noncanonical: Vec<&'static str>,
}

impl RuntimeOverride {
    fn read(path: &Path) -> Result<Option<Self>, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            // No path in the message: it ends up in startup logs.
            Err(error) => {
                return Err(ConfigError::Message(format!(
                    "cannot read config_override.json: {}",
                    error.kind()
                )))
            }
        };
        Ok(Some(Self::from_text(&text)?))
    }

    /// Both views (the `Config` and the canonical-spelling check) come from
    /// the same bytes.
    fn from_text(text: &str) -> Result<Self, ConfigError> {
        let config = Config::builder()
            .add_source(config::File::from_str(text, config::FileFormat::Json))
            .build()?;
        let noncanonical = if spelling_guard_disabled_for_test() {
            Vec::new()
        } else {
            noncanonical_keyed_sections(text)
        };
        Ok(Self {
            config,
            noncanonical,
        })
    }
}

#[cfg(test)]
thread_local! {
    /// Lets a test switch the spelling guard off on its own thread, so the
    /// single-read part of the fix can be checked through the real load path.
    static SPELLING_GUARD_DISABLED_FOR_TEST: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn spelling_guard_disabled_for_test() -> bool {
    SPELLING_GUARD_DISABLED_FOR_TEST.with(std::cell::Cell::get)
}

#[cfg(not(test))]
#[inline(always)]
fn spelling_guard_disabled_for_test() -> bool {
    false
}

/// Sections of the raw override where a key could fold into another one.
///
/// `config` lowercases keys and splits dotted top-level keys while merging,
/// and its tables are `HashMap`s: two spellings that fold to the same key
/// (`base_url` / `BASE_URL`, `embedding` / `Embedding`, `embedding` /
/// `embedding.oneapi`) are merged in a random order, so which one wins can
/// change from one load to the next. The `PUT /api/v1/config` schema only
/// writes lower-case keys, so any other spelling under `gateway` or
/// `embedding` (at any depth, including the section name itself) is treated
/// as untrusted and the deployment key is not paired with that section.
fn noncanonical_keyed_sections(text: &str) -> Vec<&'static str> {
    let Ok(serde_json::Value::Object(root)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        // `config` accepted the file, so this should not happen; fail closed.
        return KEYED_OVERRIDE_SECTIONS.to_vec();
    };
    let mut noncanonical = Vec::new();
    for &section in KEYED_OVERRIDE_SECTIONS {
        let spellings: Vec<(&String, &serde_json::Value)> = root
            .iter()
            .filter(|(key, _)| {
                key.split(['.', '['])
                    .next()
                    .is_some_and(|head| head.to_lowercase() == section)
            })
            .collect();
        let canonical = match spellings.as_slice() {
            [] => true,
            [(key, value)] => key.as_str() == section && keys_are_canonical(section, value, 0),
            // `Embedding` next to `embedding`, or `embedding.oneapi` next to it.
            _ => false,
        };
        if !canonical {
            noncanonical.push(section);
        }
    }
    noncanonical
}

/// Every object key below `value` is lower case. Exact duplicates are already
/// collapsed by the JSON parser (last one wins, as in `config`), so with all
/// keys lower case no two keys in one table can fold together.
fn keys_are_canonical(section: &str, value: &serde_json::Value, depth: usize) -> bool {
    match value {
        serde_json::Value::Object(table) => table.iter().all(|(key, child)| {
            if key.to_lowercase() != *key {
                return false;
            }
            if depth == 0 && FREE_FORM_KEY_TABLES.contains(&(section, key.as_str())) {
                return true;
            }
            keys_are_canonical(section, child, depth + 1)
        }),
        serde_json::Value::Array(items) => items
            .iter()
            .all(|item| keys_are_canonical(section, item, depth + 1)),
        _ => true,
    }
}

/// Endpoint/key pairs whose deployment key must not follow a base URL moved by
/// the runtime override:
/// `(override section, base_url path, api_key path, base_url env, api_key env)`.
const ENDPOINT_KEY_BINDINGS: &[(&str, &str, &str, &str, &str)] = &[
    (
        "gateway",
        "gateway.base_url",
        "gateway.api_key",
        "AGENT_OS_GATEWAY_BASE_URL",
        "AGENT_OS_GATEWAY_API_KEY",
    ),
    (
        "embedding",
        "embedding.oneapi.base_url",
        "embedding.oneapi.api_key",
        "AGENT_OS_EMBEDDING_ONEAPI_BASE_URL",
        "AGENT_OS_EMBEDDING_ONEAPI_API_KEY",
    ),
];

const ENDPOINT_KEY_BINDING_ORIGIN: &str = "endpoint key binding (config_override.json)";

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// WARN text for a non-canonical override section. Names only the section,
/// the dropped field and the environment pair: never a key, URL or path.
fn noncanonical_section_warning(
    section: &str,
    key_path: &str,
    base_env: &str,
    key_env: &str,
) -> String {
    format!(
        "config_override.json spells keys in its {section} section in a non-canonical way \
         (upper case, a dotted section name, or the section given twice); ignoring the \
         deployment {key_path} for it. Rewrite that section in lower case, or set \
         {base_env} together with {key_env}"
    )
}

/// A deployment key (config.yaml or environment) belongs to the deployment's
/// endpoint. When `config_override.json` (written at runtime by
/// `PUT /api/v1/config` or the model/embedding routes) moves a base URL to a
/// different endpoint and the environment does not set that base URL, the
/// deployment key is dropped instead of being sent to the new endpoint after a
/// restart. Only a key stored in the override itself is used with the
/// override's endpoint. Keys are never logged.
///
/// `runtime_override` is the same object the main builder merges, so the
/// base URL checked here is the one the merged configuration ends up with
/// (`config` lowercases key paths, so `BASE_URL`, `OneApi.Base_Url` and
/// `base_url` all name the same field). A section whose raw spelling is not
/// canonical never keeps the deployment key at all (fail closed), whatever
/// endpoint it names (#303 review).
fn bind_deployment_keys_to_their_endpoints(
    mut builder: ConfigBuilder<DefaultState>,
    deployment: &Config,
    runtime_override: Option<&RuntimeOverride>,
    env: &[(String, String)],
) -> Result<ConfigBuilder<DefaultState>, ConfigError> {
    let Some(runtime_override) = runtime_override else {
        return Ok(builder);
    };
    let overrides = &runtime_override.config;
    let override_string = |path: &str| overrides.get_string(path).ok();
    let env_value = |name: &str| {
        env.iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    };
    for (section, base_path, key_path, base_env, key_env) in ENDPOINT_KEY_BINDINGS {
        // An environment base URL beats the override (set_override wins over
        // every file source), so the key stays paired with the deployment's
        // own endpoint.
        if env_value(base_env).is_some() {
            continue;
        }
        let noncanonical = runtime_override.noncanonical.contains(section);
        // A base URL that is present but not a string (`null`, an array or
        // an object) is a moved endpoint: fail closed instead of skipping the
        // binding because `get_string` fails on it (#303 re-review).
        let non_string_base = overrides.get::<Value>(base_path).is_ok_and(|value| {
            matches!(
                value.kind,
                ValueKind::Nil | ValueKind::Array(_) | ValueKind::Table(_)
            )
        });
        if !noncanonical && !non_string_base {
            let override_base = override_string(base_path);
            let Some(override_base) = non_blank(override_base.as_deref()) else {
                continue;
            };
            let deployment_base = deployment.get_string(base_path).unwrap_or_default();
            let deployment_base = normalize_api_base(&deployment_base);
            if !deployment_base.is_empty() && deployment_base == normalize_api_base(override_base) {
                continue;
            }
        }
        let override_key = override_string(key_path);
        let override_key = non_blank(override_key.as_deref());
        let deployment_key_present = non_blank(env_value(key_env)).is_some()
            || non_blank(deployment.get_string(key_path).ok().as_deref()).is_some();
        let key = match override_key {
            Some(key) => key.to_string(),
            None => {
                if deployment_key_present && noncanonical {
                    tracing::warn!(
                        "{}",
                        noncanonical_section_warning(section, key_path, base_env, key_env)
                    );
                } else if deployment_key_present {
                    tracing::warn!(
                        "{base_path} from config_override.json is a different endpoint than the deployment's; \
                         ignoring the deployment {key_path} for it (set {base_env} together with {key_env}, \
                         or store a key for the new endpoint)"
                    );
                }
                String::new()
            }
        };
        let origin = ENDPOINT_KEY_BINDING_ORIGIN.to_string();
        builder =
            builder.set_override(*key_path, Value::new(Some(&origin), ValueKind::String(key)))?;
    }
    Ok(builder)
}

/// `load_config` with explicit layers, for tests outside this module.
#[cfg(test)]
pub(crate) fn load_config_layers_for_test(
    yaml_name: &str,
    override_path: &Path,
    env: &[(String, String)],
) -> Result<Config, ConfigError> {
    config_builder_with_sources(yaml_name, override_path, env)?.build()
}

fn load_config() -> Result<Config, ConfigError> {
    let env: Vec<(String, String)> = std::env::vars()
        .filter(|(name, _)| name.starts_with("AGENT_OS_"))
        .collect();
    WARN_UNKNOWN_ENV.call_once(|| warn_unrecognized_env_vars(&env));
    config_builder_with_sources("config", &config_override_path(), &env)?.build()
}

#[derive(Debug, Deserialize, Clone)]
pub struct Settings {
    pub gateway: GatewaySettings,
    pub memory: MemorySettings,
    pub perception: PerceptionSettings,
    pub agents: AgentSettings,
    pub api: ApiSettings,
    pub output: OutputSettings,
    pub emphasis: EmphasisConfig,
    pub logging: LoggingSettings,
    pub tool_result_router: ToolResultRouterSettings,
    #[serde(default)]
    pub embedding: EmbeddingSettings,
    #[serde(default)]
    pub token_optimization: TokenOptimizationSettings,
    #[serde(default)]
    pub batch_agents: BatchSettings,
    #[serde(default)]
    pub workspace: WorkspaceSettings,
    #[serde(default)]
    pub models: ModelsSettings,
    #[serde(default)]
    pub admin_policies: AdminPolicySettings,
    #[serde(default)]
    pub a2a: A2aSettings,
    #[serde(default)]
    pub online_corpus_watchers: OnlineCorpusWatcherSettings,
    /// Optional operator price table for invocation cost (#337).
    #[serde(default)]
    pub pricing: PricingSettings,
}

/// Operator-configured unit prices used to compute an invocation's `cost`
/// when the gateway reports none (`usage.cost_source = config_price_table`).
///
/// This is an optional operator input, not kernel pricing: the kernel ships
/// no prices, and the table is empty by default. With neither a
/// gateway-reported cost nor a table entry for every model a run used, the
/// invocation cannot succeed (`incomplete_usage`).
#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(try_from = "RawPricingSettings")]
pub struct PricingSettings {
    /// Model name exactly as the upstream reports it in its response
    /// (`model`) → unit prices. Lookups are exact; no case folding.
    pub models: std::collections::HashMap<String, ModelPrice>,
}

/// The wire shape of `pricing`; unknown members (for example a misspelled
/// `modls`) are rejected instead of silently leaving the table empty.
///
/// `models` is a list of entries naming their model in a value, not a map
/// keyed by model name: the configuration loader lower-cases map keys, which
/// would break exact matching and silently merge names that differ only in
/// letter case.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPricingSettings {
    #[serde(default)]
    models: Vec<RawModelPrice>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModelPrice {
    model: String,
    input_usd_per_million_tokens: f64,
    output_usd_per_million_tokens: f64,
}

impl TryFrom<RawPricingSettings> for PricingSettings {
    type Error = String;

    /// Rejects at load time (so the server does not start) any price that is
    /// negative, NaN or infinite, an empty model name, a model listed twice,
    /// and model names that differ only in letter case (ambiguous to
    /// operators).
    fn try_from(raw: RawPricingSettings) -> Result<Self, Self::Error> {
        let mut models = std::collections::HashMap::new();
        let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for entry in raw.models {
            let name = entry.model;
            if name.trim().is_empty() {
                return Err("pricing.models: model name must not be empty".to_string());
            }
            let price = ModelPrice {
                input_usd_per_million_tokens: entry.input_usd_per_million_tokens,
                output_usd_per_million_tokens: entry.output_usd_per_million_tokens,
            };
            if !price.is_valid() {
                return Err(format!(
                    "pricing.models `{name}`: prices must be finite and >= 0"
                ));
            }
            if let Some(other) = seen.insert(name.to_ascii_lowercase(), name.clone()) {
                return Err(if other == name {
                    format!("pricing.models: `{name}` is listed twice")
                } else {
                    format!(
                        "pricing.models: `{other}` and `{name}` differ only in letter case; model names are matched exactly, keep one"
                    )
                });
            }
            models.insert(name, price);
        }
        Ok(Self { models })
    }
}

/// Unit prices of one model, in USD per one million tokens.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelPrice {
    pub input_usd_per_million_tokens: f64,
    pub output_usd_per_million_tokens: f64,
}

impl ModelPrice {
    /// Both prices are finite and non-negative.
    pub fn is_valid(&self) -> bool {
        [
            self.input_usd_per_million_tokens,
            self.output_usd_per_million_tokens,
        ]
        .iter()
        .all(|p| p.is_finite() && *p >= 0.0)
    }
}

impl PricingSettings {
    /// The price entry for exactly `model` (the model name the upstream
    /// returned). No case folding, prefix or alias matching.
    pub fn price_for(&self, model: &str) -> Option<&ModelPrice> {
        self.models.get(model).filter(|price| price.is_valid())
    }
}

/// Deploy-time registrations for the claims-scoped online corpus job watcher.
///
/// Watchers are enabled unless this section explicitly sets `enabled: false`.
/// Each registration supplies a stable source version; changing that version
/// makes one new job eligible for the registration's declared claims scope.
#[derive(Debug, Deserialize, Clone)]
pub struct OnlineCorpusWatcherSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_watcher_poll_interval_seconds")]
    pub poll_interval_seconds: u64,
    #[serde(default = "default_watcher_max_concurrent_polls")]
    pub max_concurrent_polls: usize,
    #[serde(default = "default_watcher_queue_capacity")]
    pub queue_capacity: usize,
    #[serde(default)]
    pub registrations: Vec<OnlineCorpusWatcherRegistration>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OnlineCorpusWatcherRegistration {
    pub id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub source_id: String,
    pub source_version: String,
    #[serde(default)]
    pub source_uri: Option<String>,
    pub tenant_id: String,
    pub project_id: String,
    pub actor_id: String,
}

impl Default for OnlineCorpusWatcherSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_seconds: default_watcher_poll_interval_seconds(),
            max_concurrent_polls: default_watcher_max_concurrent_polls(),
            queue_capacity: default_watcher_queue_capacity(),
            registrations: Vec::new(),
        }
    }
}

fn default_watcher_poll_interval_seconds() -> u64 {
    60
}

fn default_watcher_max_concurrent_polls() -> usize {
    4
}

fn default_watcher_queue_capacity() -> usize {
    100
}

/// Outbound-only A2A transport configuration. This intentionally does not
/// expose inbound Agent Card or task-serving capabilities.
#[derive(Debug, Deserialize, Clone, Default)]
pub struct A2aSettings {
    #[serde(default)]
    pub outbound: A2aOutboundSettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct A2aOutboundSettings {
    /// Master switch; disabled by default so existing deployments make no A2A calls.
    #[serde(default)]
    pub enabled: bool,
    /// A remote A2A HTTP+JSON base URL. The adapter posts to `/message:send`.
    #[serde(default)]
    pub endpoint: String,
    /// Optional service credential for the remote agent. Do not use an end-user JWT here.
    #[serde(default)]
    pub bearer_token: String,
    #[serde(default = "default_a2a_timeout_seconds")]
    pub timeout_seconds: u64,
}

impl Default for A2aOutboundSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            bearer_token: String::new(),
            timeout_seconds: default_a2a_timeout_seconds(),
        }
    }
}

fn default_a2a_timeout_seconds() -> u64 {
    15
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct AdminPolicySettings {
    #[serde(default)]
    pub iam: IamPolicySettings,
    #[serde(default)]
    pub security: SecurityPolicySettings,
    #[serde(default)]
    pub storage: StoragePolicySettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct IamPolicySettings {
    #[serde(default = "default_access_token_hours")]
    pub access_token_hours: u64,
    #[serde(default = "default_refresh_token_days")]
    pub refresh_token_days: u64,
    #[serde(default = "default_true")]
    pub mfa_for_sensitive_actions: bool,
}

impl Default for IamPolicySettings {
    fn default() -> Self {
        Self {
            access_token_hours: default_access_token_hours(),
            refresh_token_days: default_refresh_token_days(),
            mfa_for_sensitive_actions: true,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct SecurityPolicySettings {
    #[serde(default = "default_true")]
    pub prompt_injection_protection: bool,
    #[serde(default = "default_hallucination_threshold")]
    pub hallucination_threshold: f64,
    #[serde(default = "default_max_tool_calls")]
    pub max_tool_calls: u64,
    #[serde(default = "default_true")]
    pub pii_redaction: bool,
}

impl Default for SecurityPolicySettings {
    fn default() -> Self {
        Self {
            prompt_injection_protection: true,
            hallucination_threshold: default_hallucination_threshold(),
            max_tool_calls: default_max_tool_calls(),
            pii_redaction: true,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct StoragePolicySettings {
    #[serde(default = "default_task_retention_days")]
    pub task_retention_days: u64,
    #[serde(default = "default_session_retention_hours")]
    pub session_retention_hours: u64,
    #[serde(default = "default_audit_retention_days")]
    pub audit_retention_days: u64,
}

impl Default for StoragePolicySettings {
    fn default() -> Self {
        Self {
            task_retention_days: default_task_retention_days(),
            session_retention_hours: default_session_retention_hours(),
            audit_retention_days: default_audit_retention_days(),
        }
    }
}

fn default_access_token_hours() -> u64 {
    2
}
fn default_refresh_token_days() -> u64 {
    7
}
fn default_hallucination_threshold() -> f64 {
    0.85
}
fn default_max_tool_calls() -> u64 {
    20
}
fn default_task_retention_days() -> u64 {
    90
}
fn default_session_retention_hours() -> u64 {
    72
}
fn default_audit_retention_days() -> u64 {
    365
}

#[derive(Debug, Deserialize, Clone)]
pub struct WorkspaceSettings {
    /// Workspace root directory path, uses process CWD if empty
    pub root: Option<String>,
    /// File scan exclusion patterns
    pub exclude_patterns: Vec<String>,
    /// Whether to enable filesystem watching
    pub watch_enabled: bool,
    /// Content cache maximum bytes
    pub content_store_max_bytes: usize,
    /// LRU content cache capacity (number of files).
    #[serde(default = "default_content_cache_capacity")]
    pub content_cache_capacity: usize,
    /// Polling interval in ms (fallback when native watching unavailable).
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    /// Debounce window in ms for file events.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    /// Maximum debounce wait in ms.
    #[serde(default = "default_max_debounce_wait_ms")]
    pub max_debounce_wait_ms: u64,
}

fn default_content_cache_capacity() -> usize {
    1000
}
fn default_poll_interval_ms() -> u64 {
    5000
}
fn default_debounce_ms() -> u64 {
    500
}
fn default_max_debounce_wait_ms() -> u64 {
    5000
}

impl Default for WorkspaceSettings {
    fn default() -> Self {
        Self {
            root: None,
            exclude_patterns: vec![
                "node_modules/".into(),
                "target/".into(),
                ".git/".into(),
                "dist/".into(),
                "build/".into(),
                "__pycache__/".into(),
                ".venv/".into(),
                "venv/".into(),
                ".next/".into(),
                "data/".into(),
                ".wild-agent-os/".into(),
                ".gliding_horse/".into(),
            ],
            watch_enabled: true,
            content_store_max_bytes: 64 * 1024 * 1024,
            content_cache_capacity: default_content_cache_capacity(),
            poll_interval_ms: default_poll_interval_ms(),
            debounce_ms: default_debounce_ms(),
            max_debounce_wait_ms: default_max_debounce_wait_ms(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct GatewaySettings {
    pub base_url: String,
    pub api_key: String,
    pub default_model: String,
    pub timeout_seconds: u64,
    pub max_retries: u32,
    #[serde(default = "default_retry_base_ms")]
    pub retry_base_ms: u64,
    /// Route deepseek-v4-flash requests through the Responses API (`/v1/responses`)
    /// instead of chat completions. Other models keep using chat completions.
    #[serde(default)]
    pub use_responses_api: bool,
    pub model_mapping: std::collections::HashMap<String, String>,
}

fn default_retry_base_ms() -> u64 {
    500
}

#[derive(Debug, Deserialize, Clone)]
pub struct MemorySettings {
    pub l0: L0Settings,
    pub l1: L1Settings,
    pub l2: L2Settings,
    pub l3: L3Settings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct L0Settings {
    pub path: String,
    pub max_entries: u64,
    pub compression: bool,
}

#[derive(Debug, Deserialize, Clone)]
pub struct L1Settings {
    pub max_messages: usize,
    pub compression_threshold: usize,
    pub max_tokens: usize,
    #[serde(default)]
    pub max_memory_mb: u64,
    /// Override default L1 eviction recency weight (None = role-specific default).
    #[serde(default)]
    pub eviction_recency_weight: Option<f64>,
    /// Override default L1 eviction relevance weight.
    #[serde(default)]
    pub eviction_relevance_weight: Option<f64>,
    /// Override default L1 eviction cost weight.
    #[serde(default)]
    pub eviction_cost_weight: Option<f64>,
    /// Override default L1 eviction relevance threshold.
    #[serde(default)]
    pub eviction_relevance_threshold: Option<f64>,
    /// Override default L1 eviction safe window in seconds.
    #[serde(default)]
    pub eviction_safe_window_seconds: Option<i64>,
    /// Override default L1 eviction beta fusion weight.
    #[serde(default)]
    pub eviction_beta: Option<f64>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct L2Settings {
    pub max_node_size: usize,
    pub max_projection_size: usize,
    #[serde(default)]
    pub max_memory_mb: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct L3Settings {
    pub default_frame: String,
    pub max_size: usize,
    #[serde(default)]
    pub max_memory_mb: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PerceptionSettings {
    pub enabled: bool,
    pub triggers: Vec<String>,
    pub cache_ttl_seconds: u64,
    pub cache_max_entries: usize,
    pub anomaly_dedup_window_seconds: u64,
    #[serde(default = "default_simple_threshold")]
    pub simple_input_threshold: usize,
    #[serde(default = "default_medium_threshold")]
    pub medium_input_threshold: usize,
    #[serde(default = "default_cycle_timeout_secs")]
    pub cycle_timeout_secs: u64,
    #[serde(default = "default_max_iterations_before_alert")]
    pub max_iterations_before_alert: usize,
    #[serde(default = "default_error_rate_threshold")]
    pub error_rate_threshold: f64,
}

fn default_simple_threshold() -> usize {
    50
}
fn default_medium_threshold() -> usize {
    200
}
fn default_cycle_timeout_secs() -> u64 {
    300
}
fn default_max_iterations_before_alert() -> usize {
    10
}
fn default_error_rate_threshold() -> f64 {
    0.5
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentSettings {
    pub max_iterations: u32,
    pub parallel_execution: bool,
    pub max_parallel_agents: usize,
    pub timeout_seconds: u64,
    pub api_timeout_seconds: u64,
    pub event_bus_capacity: usize,
    pub template_path: Option<String>,
    #[serde(default = "default_max_pdca_cycles")]
    pub max_pdca_cycles: u32,
    /// Maximum number of concurrently active methodologies (MethodologyGate).
    #[serde(default = "default_max_active")]
    pub max_active: usize,
    /// TimelineStore: take a full snapshot every N mutations.
    #[serde(default = "default_snapshot_frequency")]
    pub snapshot_frequency: u64,
    /// TimelineStore: maximum full snapshots to retain.
    #[serde(default = "default_max_full_snapshots")]
    pub max_full_snapshots: usize,
    /// L3 ProjectionEngine maximum projection size.
    #[serde(default = "default_max_projection_size")]
    pub max_projection_size: usize,
    /// SA intervention/LLM execution timeout in seconds (default 30).
    #[serde(default = "default_sa_execution_timeout_secs")]
    pub sa_execution_timeout_secs: u64,
    /// Tool executor HTTP call timeout in seconds (default 60).
    #[serde(default = "default_tool_timeout_secs")]
    pub tool_timeout_secs: u64,
    /// MCP client call timeout in seconds (default 30).
    #[serde(default = "default_mcp_timeout_secs")]
    pub mcp_timeout_secs: u64,
    /// Embedding service call timeout in seconds (default 30).
    #[serde(default = "default_embedding_timeout_secs")]
    pub embedding_timeout_secs: u64,
}

fn default_max_pdca_cycles() -> u32 {
    7
}
fn default_max_active() -> usize {
    20
}
fn default_snapshot_frequency() -> u64 {
    1000
}
fn default_max_full_snapshots() -> usize {
    10
}
fn default_max_projection_size() -> usize {
    500
}
fn default_sa_execution_timeout_secs() -> u64 {
    30
}
fn default_tool_timeout_secs() -> u64 {
    60
}
fn default_mcp_timeout_secs() -> u64 {
    30
}
fn default_embedding_timeout_secs() -> u64 {
    30
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_iterations: 10,
            parallel_execution: true,
            max_parallel_agents: 10,
            timeout_seconds: 300,
            api_timeout_seconds: 120,
            event_bus_capacity: 100,
            template_path: None,
            max_pdca_cycles: 7,
            max_active: 20,
            snapshot_frequency: 1000,
            max_full_snapshots: 10,
            max_projection_size: 500,
            sa_execution_timeout_secs: 30,
            tool_timeout_secs: 60,
            mcp_timeout_secs: 30,
            embedding_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ApiSettings {
    pub grpc_addr: String,
    pub http_addr: String,
    pub enable_metrics: bool,
    /// Kept for config/env compatibility (`AGENT_OS_API_METRICS_PORT`). No listener
    /// binds this port; Prometheus text is served as `GET /metrics` on `http_addr` (#324).
    pub metrics_port: u16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OutputSettings {
    pub directory: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct EmphasisConfig {
    pub enabled: bool,
    pub extraction_prompt: String,
    pub max_items: usize,
    pub dedup_threshold: f64,
}

impl Default for EmphasisConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extraction_prompt: r#"## Emphasis Content Extraction
If the user input contains emphatic content (such as "must", "important", "don't forget", "critical", etc.),
please extract these and place them in the "emphasis" field of the JSON (a string array).

Example:
{
  "thought": "The user emphasized that async must be used...",
  "content": "Okay, I will...",
  "summary": "Confirmed async implementation",
  "emphasis": ["must use async implementation"]
}"#.to_string(),
            max_items: 50,
            dedup_threshold: 0.85,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoggingSettings {
    pub level: String,
    pub format: String,
    pub console_output: bool,
    pub file_output: FileOutputSettings,
    pub filters: Vec<LogFilter>,
    pub sensitive_fields: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct FileOutputSettings {
    pub enabled: bool,
    pub path: String,
    pub prefix: String,
    pub rotation: String,
    pub max_files: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LogFilter {
    pub module: String,
    pub level: String,
}

impl LoggingSettings {
    pub fn test_default(prefix: &str) -> Self {
        Self {
            level: "debug".to_string(),
            format: "text".to_string(),
            console_output: true,
            file_output: FileOutputSettings {
                enabled: true,
                path: "./logs".to_string(),
                prefix: prefix.to_string(),
                rotation: "daily".to_string(),
                max_files: 10,
            },
            filters: vec![
                LogFilter {
                    module: "wild_agent_os_core::core".to_string(),
                    level: "debug".to_string(),
                },
                LogFilter {
                    module: "wild_agent_os_core::gateway".to_string(),
                    level: "debug".to_string(),
                },
                LogFilter {
                    module: "wild_agent_os_core::memory".to_string(),
                    level: "info".to_string(),
                },
                LogFilter {
                    module: "wild_agent_os_core::tools".to_string(),
                    level: "info".to_string(),
                },
                LogFilter {
                    module: "redb".to_string(),
                    level: "warn".to_string(),
                },
            ],
            sensitive_fields: vec!["api_key".to_string(), "password".to_string()],
        }
    }
}

impl Default for LoggingSettings {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: "text".to_string(),
            console_output: true,
            file_output: FileOutputSettings {
                enabled: true,
                path: "./logs".to_string(),
                prefix: "agent_os".to_string(),
                rotation: "daily".to_string(),
                max_files: 30,
            },
            filters: vec![
                LogFilter {
                    module: "wild_agent_os_core::gateway".to_string(),
                    level: "debug".to_string(),
                },
                LogFilter {
                    module: "wild_agent_os_core::core".to_string(),
                    level: "debug".to_string(),
                },
            ],
            sensitive_fields: vec![
                "api_key".to_string(),
                "password".to_string(),
                "token".to_string(),
                "secret".to_string(),
            ],
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ToolResultRouterSettings {
    pub enabled: bool,
    pub threshold_small: usize,
    pub threshold_large: usize,
    pub micro_tool_threshold: usize,
    pub preview_size: usize,
    pub max_graph_entities: usize,
    pub max_micro_tools: usize,
    pub sparql_query_timeout_ms: u64,
    pub auto_cleanup: bool,
    /// Persist and register micro-tool when PassThrough result exceeds this byte size,
    /// preparing for reference-based reclamation under context pressure.
    #[serde(default = "default_prepare_threshold")]
    pub prepare_threshold: usize,
}

fn default_prepare_threshold() -> usize {
    3072
}

impl Default for ToolResultRouterSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            threshold_small: 16384,
            threshold_large: 32768,
            micro_tool_threshold: 16384,
            preview_size: 2000,
            max_graph_entities: 500,
            max_micro_tools: 5,
            sparql_query_timeout_ms: 100,
            auto_cleanup: true,
            prepare_threshold: default_prepare_threshold(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct EmbeddingSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub ollama: OllamaEmbeddingConfig,
    #[serde(default)]
    pub oneapi: OneApiEmbeddingConfig,
    #[serde(default)]
    pub fallback: FallbackEmbeddingConfig,
}

fn default_true() -> bool {
    true
}
fn default_provider() -> String {
    "ollama".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct OllamaEmbeddingConfig {
    #[serde(default = "default_ollama_url")]
    pub base_url: String,
    #[serde(default = "default_ollama_model")]
    pub model: String,
    #[serde(default = "default_ollama_dim")]
    pub dimension: usize,
}

fn default_ollama_url() -> String {
    "http://localhost:11434".to_string()
}
fn default_ollama_model() -> String {
    "nomic-embed-text".to_string()
}
fn default_ollama_dim() -> usize {
    768
}

impl Default for OllamaEmbeddingConfig {
    fn default() -> Self {
        Self {
            base_url: default_ollama_url(),
            model: default_ollama_model(),
            dimension: default_ollama_dim(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct OneApiEmbeddingConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_oneapi_model")]
    pub model: String,
    #[serde(default = "default_oneapi_dim")]
    pub dimension: usize,
}

fn default_oneapi_model() -> String {
    "text-embedding-3-small".to_string()
}
fn default_oneapi_dim() -> usize {
    1536
}

impl Default for OneApiEmbeddingConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key: String::new(),
            model: default_oneapi_model(),
            dimension: default_oneapi_dim(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct FallbackEmbeddingConfig {
    #[serde(default = "default_fallback_dim")]
    pub dimension: usize,
}

fn default_fallback_dim() -> usize {
    128
}

impl Default for FallbackEmbeddingConfig {
    fn default() -> Self {
        Self {
            dimension: default_fallback_dim(),
        }
    }
}

impl Default for EmbeddingSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: default_provider(),
            ollama: OllamaEmbeddingConfig::default(),
            oneapi: OneApiEmbeddingConfig::default(),
            fallback: FallbackEmbeddingConfig::default(),
        }
    }
}

// ── Models 注册表(P3):多 provider + 多 resource,支持多模态 ──
/// 模型资源注册表:外部 API provider 与其下型号(resource)的集合。
/// 采用 Vec + serde(default),使旧 override 无 `models` 段时反序列化为空,零破坏。
#[derive(Debug, Deserialize, Clone, Default)]
pub struct ModelsSettings {
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub resources: Vec<ModelResource>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ProviderConfig {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub base_url: String,
    /// 反序列化保留;GET 快照脱敏为 api_key_configured,绝不回显明文。
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_provider_kind")]
    pub kind: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_provider_timeout")]
    pub timeout_seconds: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ModelResource {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub provider_id: String,
    /// 真实型号名,发给 provider 的 "model"。
    pub model: String,
    /// chat|vision|embedding|audio_asr|audio_tts|realtime
    #[serde(default)]
    pub modalities: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub context_window: Option<u32>,
    /// 向量维度(仅 embedding 模态有意义)；桥接为生效向量时写入 embedding 配置。
    #[serde(default)]
    pub dimension: Option<usize>,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_reasoning: bool,
    #[serde(default)]
    pub supports_vision: bool,
}

fn default_provider_kind() -> String {
    "openai_compatible".to_string()
}
fn default_provider_timeout() -> u64 {
    60
}

/// 归一化 OpenAI 兼容 base_url：去尾部斜杠，并剥离用户可能多写的尾部 `/v1`
/// （各调用点统一再拼 `/v1/...`，避免出现 `/v1/v1/...` 导致 404）。
pub fn normalize_api_base(base: &str) -> String {
    let t = base.trim().trim_end_matches('/');
    let t = if t.to_ascii_lowercase().ends_with("/v1") {
        &t[..t.len() - 3]
    } else {
        t
    };
    t.trim_end_matches('/').to_string()
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct TokenOptimizationSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tool_groups: ToolGroupSettings,
    #[serde(default)]
    pub tool_result_compressor: ToolResultCompressorSettings,
    #[serde(default)]
    pub context_window: ContextWindowSettings,
    #[serde(default)]
    pub tool_result_aging: ToolResultAgingSettings,
    #[serde(default)]
    pub prompt_optimization: PromptOptimizationSettings,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ToolResultCompressorSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_full_results")]
    pub max_full_results: usize,
    #[serde(default = "default_max_summary_length")]
    pub max_summary_length: usize,
    #[serde(default = "default_compression_trigger")]
    pub compression_trigger: usize,
    /// Replace tool message with reference compression if micro-tool exists and content exceeds this byte size.
    #[serde(default = "default_compress_tool_result_threshold")]
    pub compress_tool_result_threshold: usize,
}

fn default_compress_tool_result_threshold() -> usize {
    500
}

fn default_max_full_results() -> usize {
    2
}
fn default_max_summary_length() -> usize {
    200
}
fn default_compression_trigger() -> usize {
    10
}

impl Default for ToolResultCompressorSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_full_results: default_max_full_results(),
            max_summary_length: default_max_summary_length(),
            compression_trigger: default_compression_trigger(),
            compress_tool_result_threshold: default_compress_tool_result_threshold(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ContextWindowSettings {
    #[serde(default = "default_max_messages")]
    pub max_messages: usize,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: usize,
    #[serde(default = "default_compression_ratio")]
    pub compression_ratio: f32,
    #[serde(default = "default_preserve_recent")]
    pub preserve_recent: usize,
}

fn default_max_messages() -> usize {
    30
}
fn default_max_tokens() -> usize {
    16000
}
fn default_compression_ratio() -> f32 {
    0.3
}
fn default_preserve_recent() -> usize {
    4
}

impl Default for ContextWindowSettings {
    fn default() -> Self {
        Self {
            max_messages: default_max_messages(),
            max_tokens: default_max_tokens(),
            compression_ratio: default_compression_ratio(),
            preserve_recent: default_preserve_recent(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct ToolResultAgingSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Number of full results to keep (newest N tool results remain intact)
    #[serde(default = "default_aging_keep_full")]
    pub keep_full: usize,
    /// Number of old results to attempt micro-tool references (after keep_full)
    #[serde(default = "default_aging_try_microtool")]
    pub try_microtool: usize,
    /// Compression threshold: only process tool messages exceeding this byte size
    #[serde(default = "default_aging_compress_threshold")]
    pub compress_threshold: usize,
}

fn default_aging_keep_full() -> usize {
    5
}
fn default_aging_try_microtool() -> usize {
    5
}
fn default_aging_compress_threshold() -> usize {
    500
}

impl Default for ToolResultAgingSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_full: default_aging_keep_full(),
            try_microtool: default_aging_try_microtool(),
            compress_threshold: default_aging_compress_threshold(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct PromptOptimizationSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub use_layered_prompts: bool,
    #[serde(default = "default_true")]
    pub store_specs_in_kg: bool,
}

impl Default for PromptOptimizationSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            use_layered_prompts: true,
            store_specs_in_kg: true,
        }
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct BatchSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_batch_default_model")]
    pub default_model: String,
    #[serde(default = "default_batch_temperature")]
    pub default_temperature: f32,
    #[serde(default = "default_batch_max_retries")]
    pub default_max_retries: u32,
    #[serde(default = "default_true")]
    pub inject_user_reminders: bool,
    #[serde(default = "default_true")]
    pub inject_context_summary: bool,
    #[serde(default = "default_true")]
    pub inject_related_entities: bool,
    #[serde(default)]
    pub agents: Vec<BatchAgentSettings>,
}

fn default_batch_default_model() -> String {
    "deepseek-v4-flash".to_string()
}
fn default_batch_temperature() -> f32 {
    0.1
}
fn default_batch_max_retries() -> u32 {
    3
}

#[derive(Debug, Deserialize, Clone)]
pub struct BatchAgentSettings {
    pub name: String,
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub window_type: Option<String>,
    pub window_max_messages: Option<usize>,
    pub window_max_seconds: Option<u64>,
    #[serde(default)]
    pub triggers: Vec<BatchTriggerSettings>,
    #[serde(default)]
    pub prompt_source: String,
    pub prompt_template_name: Option<String>,
    pub prompt_template_path: Option<String>,
    pub business_domain: String,
    #[serde(default)]
    pub entity_types: Vec<String>,
    #[serde(default)]
    pub relation_types: Vec<String>,
    #[serde(default)]
    pub intent_types: Vec<String>,
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub max_retries: Option<u32>,
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub emit_on: Vec<String>,
    #[serde(default = "default_true")]
    pub inject_user_reminders: bool,
    #[serde(default = "default_true")]
    pub inject_context_summary: bool,

    // Maintenance Agent specific options
    #[serde(default)]
    pub min_confidence_auto_apply: Option<f64>,
    #[serde(default)]
    pub batch_size: Option<usize>,
    #[serde(default)]
    pub max_candidates: Option<usize>,
    #[serde(default)]
    pub lookback_hours: Option<u64>,
    #[serde(default)]
    pub llm_analysis_threshold: Option<f64>,
    #[serde(default)]
    pub max_items_per_run: Option<usize>,
    #[serde(default)]
    pub max_suggestions_per_run: Option<usize>,
}

impl Default for BatchAgentSettings {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            enabled: true,
            window_type: None,
            window_max_messages: Some(5),
            window_max_seconds: Some(600),
            triggers: vec![],
            prompt_source: "HybridWithTemplate".to_string(),
            prompt_template_name: None,
            prompt_template_path: None,
            business_domain: "default".to_string(),
            entity_types: vec![],
            relation_types: vec![],
            intent_types: vec![],
            model: None,
            temperature: None,
            max_retries: None,
            timeout_seconds: None,
            emit_on: vec![],
            inject_user_reminders: true,
            inject_context_summary: true,
            min_confidence_auto_apply: None,
            batch_size: None,
            max_candidates: None,
            lookback_hours: None,
            llm_analysis_threshold: None,
            max_items_per_run: None,
            max_suggestions_per_run: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct BatchTriggerSettings {
    pub trigger_type: String,
    #[serde(default)]
    pub params: std::collections::HashMap<String, String>,
}

impl Default for BatchTriggerSettings {
    fn default() -> Self {
        Self {
            trigger_type: "WindowFull".to_string(),
            params: std::collections::HashMap::new(),
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            gateway: GatewaySettings {
                base_url: String::new(),
                api_key: String::new(),
                default_model: String::new(),
                timeout_seconds: 60,
                max_retries: 3,
                retry_base_ms: 500,
                use_responses_api: false,
                model_mapping: std::collections::HashMap::from([
                    ("planning".to_string(), "deepseek-v4-pro".to_string()),
                    ("execution".to_string(), "deepseek-v4-pro".to_string()),
                    ("analysis".to_string(), "deepseek-v4-flash".to_string()),
                    ("default".to_string(), "deepseek-v4-flash".to_string()),
                ]),
            },
            memory: MemorySettings {
                l0: L0Settings {
                    path: "./data/l0".to_string(),
                    max_entries: 1_000_000,
                    compression: true,
                },
                l1: L1Settings {
                    max_messages: 100,
                    compression_threshold: 50,
                    max_tokens: 4096,
                    max_memory_mb: 0,
                    eviction_recency_weight: None,
                    eviction_relevance_weight: None,
                    eviction_cost_weight: None,
                    eviction_relevance_threshold: None,
                    eviction_safe_window_seconds: None,
                    eviction_beta: None,
                },
                l2: L2Settings {
                    max_node_size: 5_242_880,
                    max_projection_size: 500,
                    max_memory_mb: 0,
                },
                l3: L3Settings {
                    default_frame: "summary_only".to_string(),
                    max_size: 500,
                    max_memory_mb: 0,
                },
            },
            perception: PerceptionSettings {
                enabled: true,
                triggers: vec![
                    "TaskStart".to_string(),
                    "PlanCompleted".to_string(),
                    "ProgressAnomaly".to_string(),
                    "CheckCompleted".to_string(),
                    "TaskEnd".to_string(),
                    "CycleTimeout".to_string(),
                    "AgentBlocked".to_string(),
                    "ResourceConflict".to_string(),
                    "QualityDegradation".to_string(),
                    "UserFeedback".to_string(),
                ],
                cache_ttl_seconds: 300,
                cache_max_entries: 1000,
                anomaly_dedup_window_seconds: 60,
                simple_input_threshold: 50,
                medium_input_threshold: 200,
                cycle_timeout_secs: 300,
                max_iterations_before_alert: 10,
                error_rate_threshold: 0.5,
            },
            agents: AgentSettings::default(),
            api: ApiSettings {
                grpc_addr: "127.0.0.1:50051".to_string(),
                http_addr: "0.0.0.0:8080".to_string(),
                enable_metrics: true,
                metrics_port: 9090,
            },
            output: OutputSettings {
                directory: "./data/output".to_string(),
            },
            emphasis: EmphasisConfig::default(),
            logging: LoggingSettings::default(),
            tool_result_router: ToolResultRouterSettings::default(),
            embedding: EmbeddingSettings::default(),
            token_optimization: TokenOptimizationSettings::default(),
            batch_agents: BatchSettings::default(),
            workspace: WorkspaceSettings::default(),
            models: ModelsSettings::default(),
            admin_policies: AdminPolicySettings::default(),
            a2a: A2aSettings::default(),
            online_corpus_watchers: OnlineCorpusWatcherSettings::default(),
            pricing: PricingSettings::default(),
        }
    }
}

fn config_override_path() -> std::path::PathBuf {
    std::env::var("AGENTOS_DATA_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("data"))
        .join("config_override.json")
}

fn development_config_fallback_enabled(
    profile: Option<&str>,
    allow_defaults: Option<&str>,
) -> bool {
    profile.is_some_and(|value| value.eq_ignore_ascii_case("development"))
        || allow_defaults.is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
}

/// Whether the value at `path` in `config` was injected from the mapped
/// environment variable `name` (see [`MAPPED_ENV_ORIGIN_PREFIX`]).
fn is_from_mapped_env(config: &Config, name: &str, path: &str) -> bool {
    let mut value = &config.cache;
    for segment in path.split('.') {
        match &value.kind {
            ValueKind::Table(table) => match table.get(segment) {
                Some(child) => value = child,
                None => return false,
            },
            _ => return false,
        }
    }
    value
        .origin()
        .and_then(|origin| origin.strip_prefix(MAPPED_ENV_ORIGIN_PREFIX))
        == Some(name)
}

/// Deserialize a built [`Config`], naming the field path of any error.
///
/// `config` alone reports e.g. ``missing field `max_projection_size` ``
/// without saying which section is wrong; this wraps the error so an operator
/// sees ``missing field `max_projection_size` (at `memory.l2`)``.
///
/// An error at a field set from an [`ENV_KEY_MAP`] variable is replaced by one
/// naming only the field path, the variable and the expected type: the raw
/// value is never echoed, in case a secret was put in the wrong variable.
/// Errors for values from config files keep `config`'s original wording.
fn deserialize_with_field_path<T: serde::de::DeserializeOwned>(
    config: Config,
) -> Result<T, ConfigError> {
    // Resolved up front: deserializing consumes `config`.
    let env_sourced: Vec<(&str, &str)> = ENV_KEY_MAP
        .iter()
        .filter(|(name, path)| is_from_mapped_env(&config, name, path))
        .map(|&(name, path)| (path, name))
        .collect();
    serde_path_to_error::deserialize(config).map_err(|err| {
        let path = err.path().to_string();
        let inner = err.into_inner();
        if let Some((_, name)) = env_sourced.iter().find(|(p, _)| *p == path) {
            let expected = match &inner {
                ConfigError::Type { expected, .. } => *expected,
                _ => "a valid value",
            };
            return ConfigError::Message(format!(
                "invalid value for `{path}` from environment variable `{name}`: expected {expected}"
            ));
        }
        if path.is_empty() || path == "." {
            inner
        } else {
            ConfigError::Message(format!("{inner} (at `{path}`)"))
        }
    })
}

impl Settings {
    /// Whether an explicitly marked development process may start with defaults
    /// when its configuration cannot be loaded. Production always fails closed.
    pub fn development_config_fallback_enabled() -> bool {
        development_config_fallback_enabled(
            std::env::var("AGENT_OS_CONFIG_PROFILE").ok().as_deref(),
            std::env::var("AGENT_OS_ALLOW_DEFAULT_CONFIG")
                .ok()
                .as_deref(),
        )
    }

    pub fn load() -> Result<Self, ConfigError> {
        // yaml < config_override.json (written by PUT /api/v1/config) < env,
        // see `load_config`; errors name the failing field path.
        deserialize_with_field_path(load_config()?)
    }

    /// 仅加载 embedding 段（含各字段 serde 默认值），用于运行期热切换。
    /// 相比整份 `load()`，本方法不受其它必填字段（如 api.grpc_addr）约束，
    /// 因此即便 config.yaml 缺省也能稳健读到 config_override.json 的 embedding 覆盖。
    pub fn load_embedding() -> EmbeddingSettings {
        load_config()
            .ok()
            .and_then(|c| c.get::<EmbeddingSettings>("embedding").ok())
            .unwrap_or_default()
    }

    /// 仅加载 models 段(含各字段 serde 默认值),用于运行期热更新模型注册表。
    /// 与 `load_embedding` 同范式:不受其它必填字段约束,稳健读回 config_override.json 覆盖。
    pub fn load_models() -> ModelsSettings {
        load_config()
            .ok()
            .and_then(|c| c.get::<ModelsSettings>("models").ok())
            .unwrap_or_default()
    }

    pub fn validate(&self) -> Result<(), String> {
        parse_grpc_listen_addr(&self.api.grpc_addr)?;
        if self.gateway.base_url.is_empty() {
            tracing::warn!("gateway.base_url is not set. LLM features will be unavailable until configured via UI.");
        }
        if self.gateway.api_key.trim().is_empty() {
            tracing::warn!("gateway.api_key is not set. LLM calls short-circuit with zero outbound HTTP until configured via UI or env var.");
        }

        if self.agents.max_iterations == 0 {
            return Err("agents.max_iterations must be > 0".to_string());
        }
        if self.a2a.outbound.enabled && self.a2a.outbound.endpoint.trim().is_empty() {
            return Err(
                "a2a.outbound.endpoint must be set when outbound A2A is enabled".to_string(),
            );
        }
        if self.online_corpus_watchers.poll_interval_seconds == 0
            || self.online_corpus_watchers.max_concurrent_polls == 0
            || self.online_corpus_watchers.queue_capacity == 0
        {
            return Err(
                "online_corpus_watchers poll_interval_seconds, max_concurrent_polls, and queue_capacity must be > 0"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// Parse `api.grpc_addr`. An unparseable value is a startup error.
pub fn parse_grpc_listen_addr(addr: &str) -> Result<std::net::SocketAddr, String> {
    addr.parse::<std::net::SocketAddr>()
        .map_err(|error| format!("api.grpc_addr {addr:?} is not a socket address: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[test]
    fn grpc_listen_address_defaults_to_loopback() {
        assert_eq!(Settings::default().api.grpc_addr, "127.0.0.1:50051");
        let addr = parse_grpc_listen_addr(&Settings::default().api.grpc_addr).unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 50051);
    }

    #[test]
    fn grpc_listen_address_parse_failure_is_rejected() {
        let error = parse_grpc_listen_addr("not-a-socket").expect_err("unparseable address");
        assert!(error.contains("grpc_addr"));
        assert!(error.contains("not-a-socket"));
        assert!(parse_grpc_listen_addr("").is_err());
        assert!(parse_grpc_listen_addr("127.0.0.1").is_err());

        let mut settings = Settings::default();
        settings.api.grpc_addr = "not-a-socket".to_string();
        let error = settings.validate().expect_err("startup must fail");
        assert!(error.contains("not-a-socket"));
    }

    const FAKE_KEY: &str = "test-fake-gateway-key-278";

    fn test_config(dir: &Path, env: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let env = env
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<Vec<_>>();
        config_builder_with_sources(
            dir.join("config").to_str().unwrap(),
            &dir.join("config_override.json"),
            &env,
        )?
        .build()
    }

    fn write_full_test_layers(dir: &Path) {
        // The shipped config.yaml alone (memory.l2.max_projection_size and the
        // agents.* timeouts in place since #277a); no override layer.
        std::fs::write(dir.join("config.yaml"), include_str!("../../config.yaml")).unwrap();
    }

    #[test]
    fn mapped_deployment_variables_work_without_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let env = [
            ("AGENT_OS_GATEWAY_BASE_URL", "https://gateway.example.test"),
            ("AGENT_OS_GATEWAY_API_KEY", FAKE_KEY),
            ("AGENT_OS_GATEWAY_DEFAULT_MODEL", "test-model"),
            (
                "AGENT_OS_EMBEDDING_ONEAPI_BASE_URL",
                "https://embedding.example.test",
            ),
            (
                "AGENT_OS_EMBEDDING_ONEAPI_API_KEY",
                "test-fake-embedding-key-278",
            ),
            ("AGENT_OS_API_GRPC_ADDR", "127.0.0.1:50052"),
        ];
        let config = test_config(dir.path(), &env).unwrap();
        assert_eq!(config.get::<String>("gateway.base_url").unwrap(), env[0].1);
        assert_eq!(config.get::<String>("gateway.api_key").unwrap(), env[1].1);
        assert_eq!(
            config.get::<String>("gateway.default_model").unwrap(),
            env[2].1
        );
        let embedding: EmbeddingSettings = config.get("embedding").unwrap();
        assert_eq!(embedding.oneapi.base_url, env[3].1);
        assert_eq!(embedding.oneapi.api_key, env[4].1);
        assert_eq!(config.get::<String>("api.grpc_addr").unwrap(), env[5].1);

        write_full_test_layers(dir.path());
        let settings: Settings =
            deserialize_with_field_path(test_config(dir.path(), &env).unwrap())
                .unwrap_or_else(|e| panic!("shipped config.yaml + mapped env must load: {e}"));
        assert_eq!(settings.gateway.base_url, env[0].1);
        assert_eq!(settings.gateway.api_key, FAKE_KEY);
        assert_eq!(settings.gateway.default_model, env[2].1);
        assert_eq!(settings.embedding.oneapi.base_url, env[3].1);
        assert_eq!(settings.embedding.oneapi.api_key, env[4].1);
        assert_eq!(settings.api.grpc_addr, env[5].1);
    }

    #[test]
    fn every_mapped_key_deserializes_into_its_field() {
        let dir = tempfile::tempdir().unwrap();
        write_full_test_layers(dir.path());
        let keys: Vec<_> = ENV_KEY_MAP.iter().map(|(name, _)| *name).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
        for &(name, path) in ENV_KEY_MAP {
            let value = if path == "api.enable_metrics"
                || path.ends_with("enabled")
                || path.ends_with("use_responses_api")
            {
                "false"
            } else if path.ends_with("dimension")
                || path.ends_with("port")
                || path.ends_with("seconds")
                || path.ends_with("retries")
                || path.ends_with("ms")
            {
                "17"
            } else {
                "mapped-test-value"
            };
            let settings: Settings = test_config(dir.path(), &[(name, value)])
                .unwrap()
                .try_deserialize()
                .unwrap_or_else(|_| panic!("failed to deserialize {path}"));
            let actual = match path {
                "api.enable_metrics" => settings.api.enable_metrics.to_string(),
                "api.grpc_addr" => settings.api.grpc_addr,
                "api.http_addr" => settings.api.http_addr,
                "api.metrics_port" => settings.api.metrics_port.to_string(),
                "embedding.enabled" => settings.embedding.enabled.to_string(),
                "embedding.fallback.dimension" => settings.embedding.fallback.dimension.to_string(),
                "embedding.ollama.base_url" => settings.embedding.ollama.base_url,
                "embedding.ollama.dimension" => settings.embedding.ollama.dimension.to_string(),
                "embedding.ollama.model" => settings.embedding.ollama.model,
                "embedding.oneapi.api_key" => settings.embedding.oneapi.api_key,
                "embedding.oneapi.base_url" => settings.embedding.oneapi.base_url,
                "embedding.oneapi.dimension" => settings.embedding.oneapi.dimension.to_string(),
                "embedding.oneapi.model" => settings.embedding.oneapi.model,
                "embedding.provider" => settings.embedding.provider,
                "gateway.api_key" => settings.gateway.api_key,
                "gateway.base_url" => settings.gateway.base_url,
                "gateway.default_model" => settings.gateway.default_model,
                "gateway.max_retries" => settings.gateway.max_retries.to_string(),
                "gateway.retry_base_ms" => settings.gateway.retry_base_ms.to_string(),
                "gateway.timeout_seconds" => settings.gateway.timeout_seconds.to_string(),
                "gateway.use_responses_api" => settings.gateway.use_responses_api.to_string(),
                "output.directory" => settings.output.directory,
                _ => panic!("unverified mapping: {path}"),
            };
            assert_eq!(actual, value, "{name} -> {path}");
        }
    }

    #[test]
    fn mapped_string_values_are_not_reparsed_as_numbers() {
        let dir = tempfile::tempdir().unwrap();
        write_full_test_layers(dir.path());
        for value in ["007", "1e5", "true", "12345"] {
            let settings: Settings =
                test_config(dir.path(), &[("AGENT_OS_GATEWAY_API_KEY", value)])
                    .unwrap()
                    .try_deserialize()
                    .unwrap();
            assert_eq!(settings.gateway.api_key, value);
        }
        let settings: Settings = test_config(
            dir.path(),
            &[
                ("AGENT_OS_EMBEDDING_ENABLED", "true"),
                ("AGENT_OS_GATEWAY_MAX_RETRIES", "9"),
            ],
        )
        .unwrap()
        .try_deserialize()
        .unwrap();
        assert!(settings.embedding.enabled);
        assert_eq!(settings.gateway.max_retries, 9);
    }

    #[test]
    fn invalid_mapped_env_value_error_names_the_field_path() {
        const CANARY: &str = "canary-7f3a-not-a-number";
        // Out of u16 range: fails after parsing, in `config`'s range check.
        const CANARY_OVERFLOW: &str = "73519046287";
        let dir = tempfile::tempdir().unwrap();
        write_full_test_layers(dir.path());
        for (name, path, value, expected) in [
            (
                "AGENT_OS_GATEWAY_MAX_RETRIES",
                "gateway.max_retries",
                CANARY,
                "expected an integer",
            ),
            (
                "AGENT_OS_EMBEDDING_ENABLED",
                "embedding.enabled",
                CANARY,
                "expected a boolean",
            ),
            (
                "AGENT_OS_API_METRICS_PORT",
                "api.metrics_port",
                CANARY_OVERFLOW,
                "expected an unsigned 16 bit integer",
            ),
        ] {
            let config = test_config(dir.path(), &[(name, value)]).unwrap();
            let err = deserialize_with_field_path::<Settings>(config)
                .expect_err("mistyped mapped env value must fail to load")
                .to_string();
            assert!(err.contains(&format!("`{path}`")), "{err}");
            assert!(err.contains(&format!("`{name}`")), "{err}");
            assert!(err.contains(expected), "{err}");
            // The value may be a secret put in the wrong variable.
            assert!(!err.contains(value), "raw env value echoed: {err}");
        }
    }

    #[test]
    fn invalid_config_file_value_error_is_not_attributed_to_env() {
        let dir = tempfile::tempdir().unwrap();
        let shipped = shipped_config_yaml_text();
        let broken = shipped.replacen("  max_retries: 3\n", "  max_retries: oops\n", 1);
        assert_ne!(broken, shipped, "fixture edit must apply");
        std::fs::write(dir.path().join("config.yaml"), broken).unwrap();
        let err = deserialize_with_field_path::<Settings>(test_config(dir.path(), &[]).unwrap())
            .expect_err("non-numeric max_retries must fail to load")
            .to_string();
        assert!(err.contains("(at `gateway.max_retries`)"), "{err}");
        assert!(!err.contains("AGENT_OS_"), "{err}");
        assert!(err.contains("oops"), "{err}");
    }

    /// #303 review: a deployment key (yaml or env) never follows a base URL
    /// that the runtime override moved to another endpoint.
    #[test]
    fn isolation_contract_deployment_key_does_not_follow_override_base_url() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            "gateway:\n  base_url: https://deploy.invalid/v1\n  api_key: yaml-deploy-key\n\
             embedding:\n  oneapi:\n    base_url: https://deploy-emb.invalid/v1\n    api_key: yaml-emb-key\n",
        )
        .unwrap();
        let override_file = dir.path().join("config_override.json");
        let get = |env: &[(&str, &str)], path: &str| {
            test_config(dir.path(), env)
                .unwrap()
                .get::<String>(path)
                .unwrap()
        };
        let env_keys = [
            ("AGENT_OS_GATEWAY_API_KEY", "env-deploy-key"),
            ("AGENT_OS_EMBEDDING_ONEAPI_API_KEY", "env-emb-key"),
        ];

        // No override: deployment keys as before.
        assert_eq!(get(&[], "gateway.api_key"), "yaml-deploy-key");
        assert_eq!(get(&env_keys, "gateway.api_key"), "env-deploy-key");

        // Override moved both endpoints, no key of its own: no deployment key.
        std::fs::write(
            &override_file,
            r#"{"gateway":{"base_url":"https://attacker.invalid"},
                "embedding":{"oneapi":{"base_url":"https://attacker.invalid/v1"}}}"#,
        )
        .unwrap();
        for env in [&[][..], &env_keys[..]] {
            assert_eq!(get(env, "gateway.base_url"), "https://attacker.invalid");
            assert_eq!(get(env, "gateway.api_key"), "");
            assert_eq!(get(env, "embedding.oneapi.api_key"), "");
        }

        // The environment base URL beats the override: deployment pair intact.
        let env_base = [
            ("AGENT_OS_GATEWAY_BASE_URL", "https://deploy.invalid"),
            ("AGENT_OS_GATEWAY_API_KEY", "env-deploy-key"),
        ];
        assert_eq!(get(&env_base, "gateway.base_url"), "https://deploy.invalid");
        assert_eq!(get(&env_base, "gateway.api_key"), "env-deploy-key");

        // Same endpoint, different spelling: deployment keys kept.
        std::fs::write(
            &override_file,
            r#"{"gateway":{"base_url":"https://deploy.invalid/"},
                "embedding":{"oneapi":{"base_url":"https://deploy-emb.invalid"}}}"#,
        )
        .unwrap();
        assert_eq!(get(&env_keys, "gateway.api_key"), "env-deploy-key");
        assert_eq!(get(&env_keys, "embedding.oneapi.api_key"), "env-emb-key");

        // A key stored with the moved endpoint is the only key used for it.
        std::fs::write(
            &override_file,
            r#"{"gateway":{"base_url":"https://other.invalid","api_key":"override-key"},
                "embedding":{"oneapi":{"base_url":"https://other.invalid/v1","api_key":"override-emb-key"}}}"#,
        )
        .unwrap();
        assert_eq!(get(&env_keys, "gateway.api_key"), "override-key");
        assert_eq!(
            get(&env_keys, "embedding.oneapi.api_key"),
            "override-emb-key"
        );
    }

    const DEPLOY_LAYERS_YAML: &str = "gateway:\n  base_url: https://deploy.invalid/v1\n  api_key: yaml-deploy-key\n\
         embedding:\n  oneapi:\n    base_url: https://deploy-emb.invalid/v1\n    api_key: yaml-emb-key\n";
    const DEPLOY_ENV_KEYS: [(&str, &str); 2] = [
        ("AGENT_OS_GATEWAY_API_KEY", "env-deploy-key"),
        ("AGENT_OS_EMBEDDING_ONEAPI_API_KEY", "env-emb-key"),
    ];
    const DEPLOYMENT_KEYS: [&str; 4] = [
        "yaml-deploy-key",
        "yaml-emb-key",
        "env-deploy-key",
        "env-emb-key",
    ];

    /// Override samples where two spellings fold into one endpoint field:
    /// `(base_url path, api_key path, deployment base, raw override)`.
    /// Exact duplicates cannot be written with `json!`, hence raw text.
    fn duplicate_cased_override_samples(
    ) -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
        vec![
            // Same table: `base_url` and `BASE_URL`.
            (
                "embedding.oneapi.base_url",
                "embedding.oneapi.api_key",
                "https://deploy-emb.invalid/v1",
                r#"{"embedding":{"oneapi":{"base_url":"https://deploy-emb.invalid/v1","BASE_URL":"https://atk.invalid/v1"}}}"#,
            ),
            // Top level: `embedding` and `Embedding`.
            (
                "embedding.oneapi.base_url",
                "embedding.oneapi.api_key",
                "https://deploy-emb.invalid/v1",
                r#"{"embedding":{"oneapi":{"base_url":"https://deploy-emb.invalid/v1"}},"Embedding":{"oneapi":{"base_url":"https://atk.invalid/v1"}}}"#,
            ),
            (
                "gateway.base_url",
                "gateway.api_key",
                "https://deploy.invalid/v1",
                r#"{"gateway":{"base_url":"https://deploy.invalid/v1","BASE_URL":"https://atk.invalid/v1"}}"#,
            ),
            (
                "gateway.base_url",
                "gateway.api_key",
                "https://deploy.invalid/v1",
                r#"{"gateway":{"base_url":"https://deploy.invalid/v1"},"Gateway":{"base_url":"https://atk.invalid/v1"}}"#,
            ),
        ]
    }

    const DUPLICATE_LOADS: usize = 200;

    /// #303 re-review (BLOCKER on #352): `config` folds keys that differ only
    /// in case in a random order, so an override holding both
    /// `base_url` = deployment endpoint and `BASE_URL` = another endpoint
    /// (same table, or `embedding` next to `Embedding`) resolved differently
    /// from one load to the next. The deployment key must never be paired
    /// with a non-deployment URL, and a section spelled that way never keeps
    /// the deployment key at all.
    #[test]
    fn isolation_contract_duplicate_cased_override_keys_never_pair_deployment_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.yaml"), DEPLOY_LAYERS_YAML).unwrap();
        let override_file = dir.path().join("config_override.json");
        for (base_path, key_path, deploy_base, raw) in duplicate_cased_override_samples() {
            std::fs::write(&override_file, raw).unwrap();
            for env in [&[][..], &DEPLOY_ENV_KEYS[..]] {
                for _ in 0..DUPLICATE_LOADS {
                    let config = test_config(dir.path(), env).unwrap();
                    let base = config.get_string(base_path).unwrap();
                    let key = config.get_string(key_path).unwrap();
                    assert!(
                        normalize_api_base(&base) == normalize_api_base(deploy_base)
                            || !DEPLOYMENT_KEYS.contains(&key.as_str()),
                        "{raw}: deployment key paired with a non-deployment URL"
                    );
                    assert_eq!(
                        key, "",
                        "{raw}: non-canonical section kept the deployment key"
                    );
                }
            }
        }
    }

    /// Switches the spelling guard off on this thread until dropped.
    struct SpellingGuardOff;

    impl SpellingGuardOff {
        fn new() -> Self {
            SPELLING_GUARD_DISABLED_FOR_TEST.with(|off| off.set(true));
            Self
        }
    }

    impl Drop for SpellingGuardOff {
        fn drop(&mut self) {
            SPELLING_GUARD_DISABLED_FOR_TEST.with(|off| off.set(false));
        }
    }

    /// The single-read part of the fix on its own: with the spelling guard
    /// switched off, a load through the real path
    /// (`config_builder_with_sources`, via `load_config_layers_for_test`)
    /// gives the key binding and the merged configuration the same folded
    /// override (one read and one parse per load), so a load that ends up on
    /// the other endpoint has no deployment key and a load that ends up on
    /// the deployment endpoint keeps it. If the override were read or parsed
    /// twice, the two parses would fold differently in some loads and this
    /// test would fail (#303 re-review nit: the earlier version called
    /// `builder_from_layers` directly and could not see such a regression).
    #[test]
    fn isolation_contract_override_is_read_once_per_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.yaml"), DEPLOY_LAYERS_YAML).unwrap();
        let yaml = dir.path().join("config");
        let override_file = dir.path().join("config_override.json");
        let env: Vec<(String, String)> = DEPLOY_ENV_KEYS
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        for (base_path, key_path, deploy_base, raw) in duplicate_cased_override_samples() {
            // The guard would otherwise drop the deployment key outright.
            assert!(
                !RuntimeOverride::from_text(raw)
                    .unwrap()
                    .noncanonical
                    .is_empty(),
                "{raw}"
            );
            std::fs::write(&override_file, raw).unwrap();
            let _guard_off = SpellingGuardOff::new();
            let mut moved = 0;
            for _ in 0..DUPLICATE_LOADS {
                let config =
                    load_config_layers_for_test(yaml.to_str().unwrap(), &override_file, &env)
                        .unwrap();
                let base = config.get_string(base_path).unwrap();
                let key = config.get_string(key_path).unwrap();
                if normalize_api_base(&base) == normalize_api_base(deploy_base) {
                    assert!(DEPLOYMENT_KEYS.contains(&key.as_str()), "{raw}: {key_path}");
                } else {
                    moved += 1;
                    assert_eq!(
                        key, "",
                        "{raw}: deployment key paired with a non-deployment URL"
                    );
                }
            }
            // The sample really is ambiguous (both spellings win sometimes).
            assert!(moved > 0 && moved < DUPLICATE_LOADS, "{raw}: moved {moved}");
        }
    }

    /// #303 re-review nit: an override whose `base_url` is present but not
    /// a string (`null`, an array or an object) counts as a moved endpoint
    /// in the binding itself, for `gateway` and `embedding.oneapi` alike, so
    /// the deployment key (yaml or environment) is replaced by an empty key.
    /// Checked on the bound key, not on `Settings` deserialization.
    #[test]
    fn isolation_contract_non_string_override_base_url_drops_deployment_key() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.yaml"), DEPLOY_LAYERS_YAML).unwrap();
        let override_file = dir.path().join("config_override.json");
        let non_strings = [
            serde_json::json!(null),
            serde_json::json!(["https://deploy.invalid/v1"]),
            serde_json::json!({ "url": "https://deploy.invalid/v1" }),
            serde_json::json!([]),
            serde_json::json!({}),
        ];
        let sections = [
            (
                "gateway.api_key",
                "embedding.oneapi.api_key",
                ["yaml-emb-key", "env-emb-key"],
            ),
            (
                "embedding.oneapi.api_key",
                "gateway.api_key",
                ["yaml-deploy-key", "env-deploy-key"],
            ),
        ];
        for base in &non_strings {
            for (key_path, other_key_path, other_keys) in sections {
                let raw = if key_path == "gateway.api_key" {
                    serde_json::json!({ "gateway": { "base_url": base } })
                } else {
                    serde_json::json!({ "embedding": { "oneapi": { "base_url": base } } })
                };
                std::fs::write(&override_file, raw.to_string()).unwrap();
                for (env, other_key) in [
                    (&[][..], other_keys[0]),
                    (&DEPLOY_ENV_KEYS[..], other_keys[1]),
                ] {
                    let config = test_config(dir.path(), env).unwrap();
                    let key = config.get_string(key_path).unwrap();
                    assert!(
                        !DEPLOYMENT_KEYS.contains(&key.as_str()),
                        "{raw}: {key_path}"
                    );
                    assert_eq!(key, "", "{raw}: {key_path}");
                    // The other section is untouched.
                    assert_eq!(
                        config.get_string(other_key_path).unwrap(),
                        other_key,
                        "{raw}: {other_key_path}"
                    );
                }
            }
        }
    }

    /// #303 re-review: an override written before the PUT schema was typed
    /// may hold non-lower-case keys under `gateway` / `embedding` (ollama,
    /// fallback and oneapi alike). Such a section never keeps the deployment
    /// key; a WARN names the section and the env pair, never a key or a path.
    /// The file is not rewritten. Canonical overrides are unaffected.
    #[test]
    fn isolation_contract_noncanonical_override_keys_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.yaml"), DEPLOY_LAYERS_YAML).unwrap();
        let override_file = dir.path().join("config_override.json");
        let keys = |env: &[(&str, &str)]| {
            let config = test_config(dir.path(), env).unwrap();
            (
                config.get_string("gateway.api_key").unwrap(),
                config.get_string("embedding.oneapi.api_key").unwrap(),
            )
        };
        let deployment_keys = |env: &[(&str, &str)]| {
            if env.is_empty() {
                ("yaml-deploy-key", "yaml-emb-key")
            } else {
                ("env-deploy-key", "env-emb-key")
            }
        };

        let embedding_samples = [
            r#"{"Embedding":{"provider":"oneapi"}}"#,
            r#"{"embedding":{"Provider":"oneapi"}}"#,
            r#"{"embedding":{"Ollama":{"base_url":"http://ollama.invalid:11434"}}}"#,
            r#"{"embedding":{"ollama":{"BASE_URL":"http://ollama.invalid:11434"}}}"#,
            r#"{"embedding":{"Fallback":{"dimension":8}}}"#,
            r#"{"embedding":{"fallback":{"Dimension":8}}}"#,
            // Same endpoint as the deployment, but cased: still untrusted.
            r#"{"embedding":{"oneapi":{"Base_Url":"https://deploy-emb.invalid/v1"}}}"#,
            r#"{"embedding.oneapi":{"model":"m"}}"#,
        ];
        for raw in embedding_samples {
            std::fs::write(&override_file, raw).unwrap();
            for env in [&[][..], &DEPLOY_ENV_KEYS[..]] {
                let (gateway_key, embedding_key) = keys(env);
                assert_eq!(embedding_key, "", "{raw}");
                assert_eq!(gateway_key, deployment_keys(env).0, "{raw}");
            }
            assert_eq!(std::fs::read_to_string(&override_file).unwrap(), raw);
        }
        let gateway_samples = [
            r#"{"Gateway":{"default_model":"m"}}"#,
            r#"{"gateway":{"Default_Model":"m"}}"#,
            r#"{"gateway":{"Model_Mapping":{"a":"b"}}}"#,
        ];
        for raw in gateway_samples {
            std::fs::write(&override_file, raw).unwrap();
            for env in [&[][..], &DEPLOY_ENV_KEYS[..]] {
                let (gateway_key, embedding_key) = keys(env);
                assert_eq!(gateway_key, "", "{raw}");
                assert_eq!(embedding_key, deployment_keys(env).1, "{raw}");
            }
        }

        // Canonical spelling (model names under gateway.model_mapping may be
        // upper case) and unrelated sections: deployment keys kept.
        for raw in [
            r#"{"gateway":{"default_model":"m","model_mapping":{"GPT-4":"gpt-4o"}},
                "embedding":{"provider":"oneapi","ollama":{"base_url":"http://ollama.invalid:11434"},
                             "fallback":{"dimension":8},"oneapi":{"base_url":"https://deploy-emb.invalid"}}}"#,
            r#"{"Models":{"providers":[]}}"#,
        ] {
            std::fs::write(&override_file, raw).unwrap();
            for env in [&[][..], &DEPLOY_ENV_KEYS[..]] {
                let expected = deployment_keys(env);
                assert_eq!(keys(env), (expected.0.into(), expected.1.into()), "{raw}");
            }
        }

        // An environment base URL still pins the deployment pair.
        std::fs::write(
            &override_file,
            r#"{"Gateway":{"base_url":"https://atk.invalid"}}"#,
        )
        .unwrap();
        let env_pair = [
            ("AGENT_OS_GATEWAY_BASE_URL", "https://deploy.invalid"),
            ("AGENT_OS_GATEWAY_API_KEY", "env-deploy-key"),
        ];
        let config = test_config(dir.path(), &env_pair).unwrap();
        assert_eq!(
            config.get_string("gateway.base_url").unwrap(),
            "https://deploy.invalid"
        );
        assert_eq!(
            config.get_string("gateway.api_key").unwrap(),
            "env-deploy-key"
        );

        // A key stored in the override itself is still the override's own.
        std::fs::write(
            &override_file,
            r#"{"embedding":{"OneApi":{"base_url":"https://other.invalid/v1","api_key":"override-emb-key"}}}"#,
        )
        .unwrap();
        assert_eq!(keys(&DEPLOY_ENV_KEYS).1, "override-emb-key");

        // The WARN names the section and the env pair; no key, no path.
        let (_, _, key_path, base_env, key_env) = ENDPOINT_KEY_BINDINGS[1];
        let warning = noncanonical_section_warning("embedding", key_path, base_env, key_env);
        assert!(warning.contains("embedding section"), "{warning}");
        assert!(warning.contains("embedding.oneapi.api_key"), "{warning}");
        assert!(
            warning.contains("AGENT_OS_EMBEDDING_ONEAPI_BASE_URL"),
            "{warning}"
        );
        assert!(
            warning.contains("AGENT_OS_EMBEDDING_ONEAPI_API_KEY"),
            "{warning}"
        );
        // Whatever the load logs (capture depends on the process-wide tracing
        // state shared with parallel tests) never carries a key or a path.
        std::fs::write(&override_file, embedding_samples[2]).unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_writer({
                let log = log.clone();
                move || LogWriter(log.clone())
            })
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            test_config(dir.path(), &DEPLOY_ENV_KEYS).unwrap();
        });
        let line = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        for key in DEPLOYMENT_KEYS {
            assert!(!line.contains(key), "{line}");
        }
        assert!(!line.contains(dir.path().to_str().unwrap()), "{line}");
        assert!(!line.contains("ollama.invalid"), "{line}");
    }

    #[test]
    fn mapped_env_has_precedence_over_override_and_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("config.yaml");
        let override_file = dir.path().join("config_override.json");
        std::fs::write(&yaml, "gateway:\n  base_url: yaml-value\n").unwrap();
        let read = |env: &[(&str, &str)]| {
            test_config(dir.path(), env)
                .unwrap()
                .get::<String>("gateway.base_url")
                .unwrap()
        };
        assert_eq!(read(&[]), "yaml-value");
        std::fs::write(
            &override_file,
            r#"{"gateway":{"base_url":"override-value"}}"#,
        )
        .unwrap();
        assert_eq!(read(&[]), "override-value");
        let env = [("AGENT_OS_GATEWAY_BASE_URL", "env-value")];
        assert_eq!(read(&env), "env-value");
        std::fs::remove_file(override_file).unwrap();
        assert_eq!(read(&env), "env-value");
        // An explicitly empty environment value also wins over a nonempty file key.
        std::fs::write(&yaml, "gateway:\n  api_key: yaml-key\n").unwrap();
        assert!(test_config(dir.path(), &[("AGENT_OS_GATEWAY_API_KEY", "")])
            .unwrap()
            .get::<String>("gateway.api_key")
            .unwrap()
            .is_empty());
    }

    #[derive(Clone)]
    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for LogWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn unrecognized_env_warning_lists_only_sorted_names() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_writer({
                let log = log.clone();
                move || LogWriter(log.clone())
            })
            .finish();
        let env = vec![
            ("AGENT_OS_Z_UNKNOWN".into(), FAKE_KEY.into()),
            ("AGENT_OS_A_UNKNOWN".into(), "private-value-278".into()),
            ("AGENT_OS_HTTP_PORT".into(), "1234".into()),
            ("AGENT_OS_GATEWAY_API_KEY".into(), FAKE_KEY.into()),
        ];
        tracing::subscriber::with_default(subscriber, || warn_unrecognized_env_vars(&env));
        let line = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        assert!(line.contains("AGENT_OS_A_UNKNOWN, AGENT_OS_Z_UNKNOWN"));
        assert!(line.contains("only take effect via legacy"));
        assert!(!line.contains(FAKE_KEY));
        assert!(!line.contains("private-value-278"));
        assert!(!line.contains("AGENT_OS_HTTP_PORT"));
        assert!(!line.contains("AGENT_OS_GATEWAY_API_KEY"));
    }

    #[test]
    fn default_config_fallback_requires_explicit_development_opt_in() {
        assert!(!development_config_fallback_enabled(None, None));
        assert!(development_config_fallback_enabled(
            Some("development"),
            None
        ));
        assert!(development_config_fallback_enabled(
            Some("DEVELOPMENT"),
            None
        ));
        assert!(development_config_fallback_enabled(None, Some("true")));
        assert!(development_config_fallback_enabled(None, Some("1")));
        assert!(!development_config_fallback_enabled(
            Some("production"),
            Some("false")
        ));
    }

    #[test]
    fn test_logging_settings_test_default() {
        let settings = LoggingSettings::test_default("test_prefix");
        assert_eq!(settings.level, "debug");
        assert_eq!(settings.format, "text");
        assert!(settings.console_output);
        assert!(settings.file_output.enabled);
        assert_eq!(settings.file_output.prefix, "test_prefix");
        assert!(settings
            .filters
            .iter()
            .any(|f| f.module == "redb" && f.level == "warn"));
        assert!(settings
            .filters
            .iter()
            .any(|f| f.module == "wild_agent_os_core::core" && f.level == "debug"));
        assert!(settings
            .filters
            .iter()
            .any(|f| f.module == "wild_agent_os_core::memory" && f.level == "info"));
    }

    #[test]
    fn test_logging_settings_default_has_redb_in_init() {
        let settings = LoggingSettings::default();
        assert_eq!(settings.level, "info");
    }

    #[test]
    fn test_normalize_api_base() {
        // 无 /v1：原样（仅去尾斜杠）
        assert_eq!(normalize_api_base("https://api.x.com"), "https://api.x.com");
        assert_eq!(
            normalize_api_base("https://api.x.com/"),
            "https://api.x.com"
        );
        // 含尾部 /v1：剥离，避免后续拼接出 /v1/v1
        assert_eq!(
            normalize_api_base("https://api.x.com/v1"),
            "https://api.x.com"
        );
        assert_eq!(
            normalize_api_base("https://api.x.com/v1/"),
            "https://api.x.com"
        );
        assert_eq!(
            normalize_api_base("https://api.x.com/V1"),
            "https://api.x.com"
        );
        // 带子路径的兼容端点（/v1 结尾同样剥离，其余保留）
        assert_eq!(
            normalize_api_base("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "https://dashscope.aliyuncs.com/compatible-mode"
        );
        // 路径中间的 v1 不受影响
        assert_eq!(
            normalize_api_base("https://api.x.com/v1/foo"),
            "https://api.x.com/v1/foo"
        );
    }

    #[test]
    fn test_models_settings_empty_and_full() {
        // 空对象 → 默认空注册表(向后兼容:旧 override 无 models 段)。
        let empty: ModelsSettings = serde_json::from_str("{}").unwrap();
        assert!(empty.providers.is_empty());
        assert!(empty.resources.is_empty());

        // 满配:provider 省略 kind/enabled/timeout 走默认;resource 省略布尔能力走默认。
        let full: ModelsSettings = serde_json::from_str(
            r#"{
                "providers": [{ "id": "prov-openai", "base_url": "https://api.local", "api_key": "sk-x" }],
                "resources": [{ "id": "res-vl", "provider_id": "prov-openai", "model": "qwen-vl-max", "modalities": ["chat","vision"], "supports_vision": true }]
            }"#,
        )
        .unwrap();
        assert_eq!(full.providers.len(), 1);
        let p = &full.providers[0];
        assert_eq!(p.id, "prov-openai");
        assert_eq!(p.kind, "openai_compatible"); // 默认
        assert!(p.enabled); // 默认 true
        assert_eq!(p.timeout_seconds, 60); // 默认
        assert_eq!(full.resources.len(), 1);
        let r = &full.resources[0];
        assert_eq!(r.model, "qwen-vl-max");
        assert!(r.enabled); // 默认 true
        assert!(r.supports_vision);
        assert!(!r.supports_tools); // 默认 false
        assert_eq!(r.modalities, vec!["chat".to_string(), "vision".to_string()]);
    }

    #[test]
    fn test_admin_policy_settings_defaults_and_overrides() {
        let defaults: AdminPolicySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(defaults.iam.access_token_hours, 2);
        assert_eq!(defaults.storage.session_retention_hours, 72);
        assert!(defaults.security.prompt_injection_protection);

        let configured: AdminPolicySettings = serde_json::from_str(
            r#"{
                "iam": {"access_token_hours": 4},
                "security": {"max_tool_calls": 50},
                "storage": {"task_retention_days": 180}
            }"#,
        )
        .unwrap();
        assert_eq!(configured.iam.access_token_hours, 4);
        assert_eq!(configured.iam.refresh_token_days, 7);
        assert_eq!(configured.security.max_tool_calls, 50);
        assert_eq!(configured.storage.task_retention_days, 180);
    }

    #[test]
    fn test_gateway_settings_deserializes_retry_base_ms() {
        let yaml = r#"
            base_url: "https://api.deepseek.com"
            api_key: "sk-test"
            default_model: "deepseek-v4-flash"
            timeout_seconds: 300
            max_retries: 3
            retry_base_ms: 750
            model_mapping: {}
        "#;
        let cfg = Config::builder()
            .add_source(config::File::from_str(yaml, config::FileFormat::Yaml))
            .build()
            .unwrap();
        let settings: GatewaySettings = cfg.try_deserialize().unwrap();
        assert_eq!(settings.retry_base_ms, 750);
    }

    #[test]
    fn test_gateway_settings_retry_base_ms_default() {
        let yaml = r#"
            base_url: "https://api.deepseek.com"
            api_key: "sk-test"
            default_model: "deepseek-v4-flash"
            timeout_seconds: 300
            max_retries: 3
            model_mapping: {}
        "#;
        let cfg = Config::builder()
            .add_source(config::File::from_str(yaml, config::FileFormat::Yaml))
            .build()
            .unwrap();
        let settings: GatewaySettings = cfg.try_deserialize().unwrap();
        // retry_base_ms omitted -> serde default 500
        assert_eq!(settings.retry_base_ms, 500);
    }

    #[test]
    fn test_gateway_settings_use_responses_api() {
        let yaml = r#"
            base_url: "https://api.deepseek.com"
            api_key: "sk-test"
            default_model: "deepseek-v4-flash"
            timeout_seconds: 300
            max_retries: 3
            use_responses_api: true
            model_mapping: {}
        "#;
        let cfg = Config::builder()
            .add_source(config::File::from_str(yaml, config::FileFormat::Yaml))
            .build()
            .unwrap();
        let settings: GatewaySettings = cfg.try_deserialize().unwrap();
        assert!(settings.use_responses_api);
    }

    #[test]
    fn test_gateway_settings_use_responses_api_defaults_off() {
        let yaml = r#"
            base_url: "https://api.deepseek.com"
            api_key: "sk-test"
            default_model: "deepseek-v4-flash"
            timeout_seconds: 300
            max_retries: 3
            model_mapping: {}
        "#;
        let cfg = Config::builder()
            .add_source(config::File::from_str(yaml, config::FileFormat::Yaml))
            .build()
            .unwrap();
        let settings: GatewaySettings = cfg.try_deserialize().unwrap();
        // 未配置时默认走 chat completions,保持向后兼容
        assert!(!settings.use_responses_api);
    }

    #[test]
    fn online_corpus_watchers_are_enabled_by_default_and_can_be_disabled() {
        let defaults: OnlineCorpusWatcherSettings = serde_json::from_str("{}").unwrap();
        assert!(defaults.enabled);
        assert_eq!(defaults.poll_interval_seconds, 60);

        let disabled: OnlineCorpusWatcherSettings =
            serde_json::from_str(r#"{"enabled": false}"#).unwrap();
        assert!(!disabled.enabled);
    }

    #[test]
    fn test_agent_settings_deserializes_tunables() {
        let yaml = r#"
            max_iterations: 10
            parallel_execution: true
            max_parallel_agents: 10
            timeout_seconds: 300
            api_timeout_seconds: 120
            event_bus_capacity: 100
            max_pdca_cycles: 7
            max_active: 42
            snapshot_frequency: 2000
            max_full_snapshots: 5
            max_projection_size: 1024
        "#;
        let cfg = Config::builder()
            .add_source(config::File::from_str(yaml, config::FileFormat::Yaml))
            .build()
            .unwrap();
        let settings: AgentSettings = cfg.try_deserialize().unwrap();
        assert_eq!(settings.max_active, 42);
        assert_eq!(settings.snapshot_frequency, 2000);
        assert_eq!(settings.max_full_snapshots, 5);
        assert_eq!(settings.max_projection_size, 1024);
    }

    #[test]
    fn test_agent_settings_tunables_default() {
        let yaml = r#"
            max_iterations: 10
            parallel_execution: true
            max_parallel_agents: 10
            timeout_seconds: 300
            api_timeout_seconds: 120
            event_bus_capacity: 100
        "#;
        let cfg = Config::builder()
            .add_source(config::File::from_str(yaml, config::FileFormat::Yaml))
            .build()
            .unwrap();
        let settings: AgentSettings = cfg.try_deserialize().unwrap();
        assert_eq!(settings.max_active, 20);
        assert_eq!(settings.snapshot_frequency, 1000);
        assert_eq!(settings.max_full_snapshots, 10);
        assert_eq!(settings.max_projection_size, 500);
    }

    /// The `config.yaml` shipped at the repository root, without env vars or
    /// `config_override.json`, exactly as a fresh install reads it.
    fn shipped_config_yaml_text() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.yaml");
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    fn config_from_yaml(text: &str) -> Config {
        Config::builder()
            .add_source(config::File::from_str(text, config::FileFormat::Yaml))
            .build()
            .expect("shipped config.yaml must be valid YAML")
    }

    #[test]
    fn shipped_config_yaml_deserializes_into_settings() {
        let settings: Settings =
            deserialize_with_field_path(config_from_yaml(&shipped_config_yaml_text()))
                .unwrap_or_else(|e| panic!("shipped config.yaml must load: {e}"));
        assert_eq!(settings.memory.l2.max_node_size, 2048);
        assert_eq!(settings.memory.l2.max_projection_size, 500);
        assert_eq!(settings.agents.sa_execution_timeout_secs, 30);
        assert_eq!(settings.agents.tool_timeout_secs, 60);
        assert_eq!(settings.agents.mcp_timeout_secs, 30);
        assert_eq!(settings.agents.embedding_timeout_secs, 30);
    }

    #[test]
    fn shipped_config_yaml_has_no_keys_outside_settings() {
        let mut ignored = Vec::new();
        let _settings: Settings =
            serde_ignored::deserialize(config_from_yaml(&shipped_config_yaml_text()), |path| {
                ignored.push(path.to_string())
            })
            .unwrap_or_else(|e| panic!("shipped config.yaml must load: {e}"));
        assert!(
            ignored.is_empty(),
            "config.yaml keys that no settings struct reads (misplaced or misspelled): {ignored:?}"
        );
    }

    #[test]
    fn config_load_error_names_the_field_path() {
        // Reproduce the pre-fix layout: `max_projection_size` one level too high.
        let shipped = shipped_config_yaml_text();
        let broken = shipped.replacen(
            "    max_node_size: 2048\n    max_projection_size: 500\n",
            "    max_node_size: 2048\n  max_projection_size: 500\n",
            1,
        );
        assert_ne!(broken, shipped, "fixture edit must apply");
        let err = deserialize_with_field_path::<Settings>(config_from_yaml(&broken))
            .expect_err("misplaced max_projection_size must fail to load")
            .to_string();
        assert!(err.contains("max_projection_size"), "{err}");
        assert!(err.contains("memory.l2"), "{err}");
    }

    fn pricing_from_yaml(yaml: &str) -> Result<PricingSettings, ConfigError> {
        Config::builder()
            .add_source(config::File::from_str(yaml, config::FileFormat::Yaml))
            .build()?
            .get::<PricingSettings>("pricing")
    }

    fn price_entry(model: &str, input: &str, output: &str) -> String {
        format!(
            "    - {{ model: \"{model}\", input_usd_per_million_tokens: {input}, output_usd_per_million_tokens: {output} }}\n"
        )
    }

    /// N4: a price table that cannot be trusted stops the load.
    #[test]
    fn pricing_rejects_invalid_prices_typos_and_case_collisions() {
        let table = |entries: &[String]| format!("pricing:\n  models:\n{}", entries.concat());
        for (yaml, needle) in [
            (table(&[price_entry("m", "-1.0", "1.0")]), "finite"),
            // YAML `.nan` / `.inf` already fail to load as numbers; a number
            // too large for f64 loads as infinity and is caught here.
            (table(&[price_entry("m", ".nan", "1.0")]), "failed"),
            (table(&[price_entry("m", "1.0", ".inf")]), "failed"),
            (table(&[price_entry("m", "1e400", "1.0")]), "finite"),
            (table(&[price_entry("m", "1.0", "-1e400")]), "finite"),
            (table(&[price_entry("", "1.0", "1.0")]), "empty"),
            (
                table(&[price_entry("A", "1.0", "1.0"), price_entry("a", "9.0", "9.0")]),
                "letter case",
            ),
            (
                table(&[price_entry("m", "1.0", "1.0"), price_entry("m", "2.0", "2.0")]),
                "listed twice",
            ),
            ("pricing:\n  modls: []\n".to_string(), "modls"),
            (
                "pricing:\n  models:\n    - { model: m, input_usd_per_million_tokens: 1.0, output_usd_per_millon_tokens: 1.0 }\n".to_string(),
                "output_usd_per_millon_tokens",
            ),
        ] {
            let err = pricing_from_yaml(&yaml).expect_err(&yaml).to_string();
            assert!(err.contains(needle), "{yaml} -> {err}");
        }
    }

    /// Lookups use the upstream's model name exactly, with its letter case
    /// kept through loading.
    #[test]
    fn pricing_matches_model_names_exactly() {
        let pricing = pricing_from_yaml(&format!(
            "pricing:\n  models:\n{}{}",
            price_entry("GPT-4.1", "2.0", "8.0"),
            price_entry("vendor/Model-X", "0", "0.5"),
        ))
        .unwrap();
        let price = pricing.price_for("GPT-4.1").expect("exact name");
        assert_eq!(price.input_usd_per_million_tokens, 2.0);
        assert_eq!(price.output_usd_per_million_tokens, 8.0);
        assert!(pricing.price_for("gpt-4.1").is_none());
        assert!(pricing.price_for("GPT-4.1-mini").is_none());
        assert!(pricing.price_for("vendor/Model-X").is_some());
        assert!(pricing.price_for("vendor/model-x").is_none());
        assert_eq!(pricing_from_yaml("pricing: {}\n").unwrap().models.len(), 0);
    }
}
