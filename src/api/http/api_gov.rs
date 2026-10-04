/*!
 * 对外发布与 API 密钥治理（入站密钥）子系统。
 *
 * 领域模型：调用方 Client（限流/配额/scope 授权）── N ── ApiKey（凭据，仅存哈希）。
 * 与出站网关密钥（gateway.api_key，平台→LLM）互不相关。
 *
 * 持久化：data/api_clients.json、data/api_keys.json（pretty JSON），
 *         data/api_audit.jsonl（滚动追加）。
 *
 * 锁顺序 / Lock order: `AppState::api_keys` and `AppState::api_clients` are
 * separate locks. Any caller that holds both must take `api_keys` first, then
 * `api_clients` (e.g. before calling `resolve_bearer_token`). This module only
 * works on slices and takes neither lock itself.
 */

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashSet, path::PathBuf};

// ─── 领域模型 ──────────────────────────────────────────────────────────────────

/// 调用方限流参数（0 表示不限制）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimit {
    #[serde(default)]
    pub rpm: u32,
    #[serde(default)]
    pub concurrency: u32,
}
impl Default for RateLimit {
    fn default() -> Self {
        Self {
            rpm: 60,
            concurrency: 4,
        }
    }
}

/// 调用方配额参数（0 表示不限制）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Quota {
    #[serde(default)]
    pub daily: u64,
    #[serde(default)]
    pub monthly: u64,
}

/// 调用方（入站 API 的一等实体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiClient {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_tenant")]
    pub tenant_id: String,
    #[serde(default)]
    pub owner: String,
    /// 被授权可调用的 Agent id 列表（scope）。
    #[serde(default)]
    pub granted_agent_ids: Vec<String>,
    /// active | disabled
    #[serde(default = "default_active")]
    pub status: String,
    #[serde(default)]
    pub rate_limit: RateLimit,
    #[serde(default)]
    pub quota: Quota,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

/// 入站密钥凭据（仅存 SHA-256 哈希，明文仅签发时返回一次）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub client_id: String,
    /// sk-<tenant>-<前6位>，可展示、用于日志脱敏。
    pub key_prefix: String,
    /// SHA-256(明文) hex。
    pub key_hash: String,
    /// active | revoked
    #[serde(default = "default_active")]
    pub status: String,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub created_at: String,
}

fn default_tenant() -> String {
    "default".to_string()
}
fn default_active() -> String {
    "active".to_string()
}

// ─── 持久化 ────────────────────────────────────────────────────────────────────

fn api_clients_path() -> PathBuf {
    super::data_dir().join("api_clients.json")
}
fn api_keys_path() -> PathBuf {
    super::data_dir().join("api_keys.json")
}
pub fn api_audit_path() -> PathBuf {
    super::data_dir().join("api_audit.jsonl")
}

pub fn load_api_clients() -> Vec<ApiClient> {
    let mut clients: Vec<ApiClient> = match std::fs::read_to_string(api_clients_path()) {
        Ok(c) => serde_json::from_str(&c).unwrap_or_default(),
        Err(_) => Vec::new(),
    };
    quarantine_cross_tenant_client_ids(&mut clients);
    clients
}

/// Status given to every client whose id also appears under another tenant.
/// Such clients are unusable: inbound auth rejects them with 401 and their
/// legacy (tenant-less) audit records are hidden. Fail closed.
pub const CLIENT_ID_CONFLICT_STATUS: &str = "id_conflict";

/// Client ids that appear under more than one tenant, with those tenants (sorted).
pub fn cross_tenant_client_id_collisions(
    clients: &[ApiClient],
) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    let mut tenants_by_id: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> =
        Default::default();
    for client in clients {
        tenants_by_id
            .entry(client.id.clone())
            .or_default()
            .insert(client.tenant_id.clone());
    }
    tenants_by_id.retain(|_, tenants| tenants.len() > 1);
    tenants_by_id
}

/// Marks ALL clients whose id collides across tenants as unusable and logs one
/// warning per id (id and tenant ids only). Returns the number of clients marked.
pub fn quarantine_cross_tenant_client_ids(clients: &mut [ApiClient]) -> usize {
    let collisions = cross_tenant_client_id_collisions(clients);
    if collisions.is_empty() {
        return 0;
    }
    for (id, tenants) in &collisions {
        tracing::warn!(
            client_id = %id,
            tenants = ?tenants,
            "api client id is used by more than one tenant; all of these clients are disabled (fail closed)"
        );
    }
    let mut marked = 0;
    for client in clients.iter_mut() {
        if collisions.contains_key(&client.id) {
            client.status = CLIENT_ID_CONFLICT_STATUS.to_string();
            marked += 1;
        }
    }
    marked
}

/// Client ids whose legacy audit records (no `tenant_id`) a tenant may read:
/// the tenant's own clients, minus any id that is (or was, per the conflict
/// status) shared with another tenant. Ambiguous history is never returned.
pub fn legacy_audit_client_ids(clients: &[ApiClient], tenant: &str) -> HashSet<String> {
    let collisions = cross_tenant_client_id_collisions(clients);
    clients
        .iter()
        .filter(|c| c.tenant_id == tenant)
        .filter(|c| c.status != CLIENT_ID_CONFLICT_STATUS)
        .filter(|c| !collisions.contains_key(&c.id))
        .map(|c| c.id.clone())
        .collect()
}
pub fn save_api_clients(clients: &[ApiClient]) -> std::io::Result<()> {
    let path = api_clients_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let content = serde_json::to_string_pretty(clients).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
}
pub fn load_api_keys() -> Vec<ApiKey> {
    match std::fs::read_to_string(api_keys_path()) {
        Ok(c) => serde_json::from_str(&c).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}
pub fn save_api_keys(keys: &[ApiKey]) -> std::io::Result<()> {
    let path = api_keys_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let content = serde_json::to_string_pretty(keys).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&path, content)
}

/// 追加一条审计记录（JSONL）。
pub fn append_audit(entry: &Value) {
    let path = api_audit_path();
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(mut line) = serde_json::to_string(entry) {
        line.push('\n');
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// 读取审计（返回最近 limit 条，倒序），可按 client_id / agent_id 过滤。
pub fn read_audit_for_tenant(
    tenant: &str,
    tenant_client_ids: &HashSet<String>,
    client_id: Option<&str>,
    agent_id: Option<&str>,
    limit: usize,
) -> Vec<Value> {
    let content = match std::fs::read_to_string(api_audit_path()) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<Value> = content
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| match e.get("tenant_id") {
            Some(value) => value.as_str() == Some(tenant),
            None => e
                .get("client_id")
                .and_then(Value::as_str)
                .is_some_and(|id| tenant_client_ids.contains(id)),
        })
        .filter(|e| {
            client_id.is_none_or(|c| e.get("client_id").and_then(|v| v.as_str()) == Some(c))
        })
        .filter(|e| agent_id.is_none_or(|a| e.get("agent_id").and_then(|v| v.as_str()) == Some(a)))
        .collect();
    out.reverse();
    out.truncate(limit);
    out
}

// ─── 密钥生成 / 哈希 ───────────────────────────────────────────────────────────

/// SHA-256(明文) hex。
pub fn hash_key(plaintext: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(plaintext.as_bytes());
    hex::encode(h.finalize())
}

/// 生成一把新 key，返回 (明文, key_prefix, key_hash)。明文仅此一次可得。
pub fn generate_key(tenant: &str) -> (String, String, String) {
    let slug: String = tenant
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "t" } else { slug };
    let secret = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let plaintext = format!("sk-{slug}-{secret}");
    let prefix = format!("sk-{slug}-{}", &secret[..6]);
    let hash = hash_key(&plaintext);
    (plaintext, prefix, hash)
}

// ─── 鉴权解析 ──────────────────────────────────────────────────────────────────

/// 通过入站密钥解析出的调用方上下文。
#[derive(Debug, Clone)]
pub struct ApiCallerContext {
    pub client_id: String,
    pub key_id: String,
    pub key_prefix: String,
    pub tenant_id: String,
    pub owner: String,
    pub granted_agent_ids: Vec<String>,
}

/// 鉴权失败原因。
#[derive(Debug, Clone, PartialEq)]
pub enum AuthError {
    Unauthorized,
    KeyRevoked,
    KeyExpired,
    ClientDisabled,
}
impl AuthError {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthError::Unauthorized => "unauthorized",
            AuthError::KeyRevoked => "key_revoked",
            AuthError::KeyExpired => "key_expired",
            AuthError::ClientDisabled => "client_disabled",
        }
    }
}

/// 校验一个 Bearer token 是否为合法入站密钥；命中则返回调用方上下文与命中的 key_id。
/// 仅处理 `sk-` 前缀 token；其它交由上层 JWT 逻辑。
pub fn resolve_bearer_token(
    token: &str,
    keys: &[ApiKey],
    clients: &[ApiClient],
) -> Result<ApiCallerContext, AuthError> {
    if !token.starts_with("sk-") {
        return Err(AuthError::Unauthorized);
    }
    let hash = hash_key(token);
    let key = keys
        .iter()
        .find(|k| k.key_hash == hash)
        .ok_or(AuthError::Unauthorized)?;
    if key.status != "active" {
        return Err(AuthError::KeyRevoked);
    }
    if let Some(exp) = &key.expires_at {
        if let Ok(t) = chrono::DateTime::parse_from_rfc3339(exp) {
            if chrono::Utc::now() > t {
                return Err(AuthError::KeyExpired);
            }
        }
    }
    // Never take the first client with this id: a client id shared by more
    // than one client (e.g. across tenants) does not identify the owner, so
    // any duplicate fails closed with 401 regardless of status. This is the
    // primary defense; no status change can re-enable a colliding id.
    let mut candidates = clients.iter().filter(|c| c.id == key.client_id);
    let client = match (candidates.next(), candidates.next()) {
        (Some(only), None) => only,
        _ => return Err(AuthError::Unauthorized),
    };
    // Defense in depth: a key minted by `generate_key` carries its tenant's
    // slug; if that slug is not the resolved client's tenant, the key and the
    // client do not belong together -> 401. Keys without such a prefix
    // (legacy/imported) are not covered yet (needs a tenant field on keys).
    if key_has_generated_tenant_prefix(key) && !key_prefix_matches_tenant(key, &client.tenant_id) {
        return Err(AuthError::Unauthorized);
    }
    if client.status == CLIENT_ID_CONFLICT_STATUS {
        return Err(AuthError::Unauthorized);
    }
    if client.status != "active" {
        return Err(AuthError::ClientDisabled);
    }
    Ok(ApiCallerContext {
        client_id: client.id.clone(),
        key_id: key.id.clone(),
        key_prefix: key.key_prefix.clone(),
        tenant_id: client.tenant_id.clone(),
        owner: client.owner.clone(),
        granted_agent_ids: client.granted_agent_ids.clone(),
    })
}

/// Slug that `generate_key` embeds in a tenant's key prefixes.
fn tenant_key_slug(tenant: &str) -> String {
    let slug: String = tenant
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "t".to_string()
    } else {
        slug.to_string()
    }
}

/// Whether `key.key_prefix` has the exact shape `generate_key` writes for
/// `tenant` (`sk-<slug>-<6 chars>`). Slugs are lossy (case folding,
/// punctuation → `-`), so a match is not final proof of ownership; see
/// [`tenant_may_manage_key`] for the fail-closed use.
pub(crate) fn key_prefix_matches_tenant(key: &ApiKey, tenant: &str) -> bool {
    let head = format!("sk-{}-", tenant_key_slug(tenant));
    key.key_prefix.len() == head.len() + 6
        && key.key_prefix.starts_with(&head)
        && !key.key_prefix[head.len()..].contains('-')
}

/// Whether `key.key_prefix` has the exact shape `generate_key` writes for some
/// tenant: `sk-<slug>-<6 lowercase hex>`, with a non-empty slug of
/// `[a-z0-9-]` that does not start or end with `-`.
fn key_has_generated_tenant_prefix(key: &ApiKey) -> bool {
    let Some(rest) = key.key_prefix.strip_prefix("sk-") else {
        return false;
    };
    if rest.len() < 8 || !rest.is_ascii() {
        return false;
    }
    let (slug, tail) = rest.split_at(rest.len() - 7);
    let Some(hex) = tail.strip_prefix('-') else {
        return false;
    };
    hex.bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `tenant`'s key prefix cannot be told apart from another tenant that
/// shares the colliding client id (`tenants` from
/// [`cross_tenant_client_id_collisions`]).
pub(crate) fn colliding_tenant_slug_is_ambiguous(
    tenants: &std::collections::BTreeSet<String>,
    tenant: &str,
) -> bool {
    let slug = tenant_key_slug(tenant);
    tenants
        .iter()
        .any(|t| t != tenant && tenant_key_slug(t) == slug)
}

/// Management-plane key ownership for a key whose `client_id` names one of
/// `tenant`'s clients. Unique ids: every key under the id. Ids shared across
/// tenants: only keys carrying `tenant`'s prefix, and none at all when another
/// colliding tenant has the same slug (fail closed).
pub(crate) fn tenant_may_manage_key(
    collisions: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    key: &ApiKey,
    tenant: &str,
) -> bool {
    match collisions.get(&key.client_id) {
        None => true,
        Some(tenants) => {
            !colliding_tenant_slug_is_ambiguous(tenants, tenant)
                && key_prefix_matches_tenant(key, tenant)
        }
    }
}

// ─── 进程内限流 / 配额 / 并发 ──────────────────────────────────────────────────

use std::collections::HashMap;

/// 放行判定失败结果。
#[derive(Debug, Clone)]
pub enum UsageDenied {
    RateLimited { retry_after: u64 },
    QuotaExceeded { scope: &'static str },
    Concurrency,
}

#[derive(Default)]
struct UsageInner {
    rpm: HashMap<String, (u64, u32)>,  // client -> (minute_epoch, count)
    concurrency: HashMap<String, u32>, // client -> in-flight
    daily: HashMap<String, (String, u64)>, // client -> (yyyy-mm-dd, used)
    monthly: HashMap<String, (String, u64)>, // client -> (yyyy-mm, used)
}

/// 进程内用量状态（单副本足够；多副本需外置，列远期）。
#[derive(Default)]
pub struct ApiUsageState {
    inner: parking_lot::Mutex<UsageInner>,
}

/// 并发计数 RAII 守卫：drop 时归还并发额度。
pub struct ConcurrencyGuard {
    state: std::sync::Arc<ApiUsageState>,
    client_id: String,
}
impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        let mut g = self.state.inner.lock();
        if let Some(c) = g.concurrency.get_mut(&self.client_id) {
            *c = c.saturating_sub(1);
        }
    }
}

impl ApiUsageState {
    /// 校验限流+配额+并发；全部通过则提交计数并返回并发守卫。
    pub fn try_acquire(
        self: &std::sync::Arc<Self>,
        client: &ApiClient,
    ) -> Result<ConcurrencyGuard, UsageDenied> {
        let now = chrono::Utc::now();
        let minute = (now.timestamp() as u64) / 60;
        let dk = now.format("%Y-%m-%d").to_string();
        let mk = now.format("%Y-%m").to_string();
        let mut g = self.inner.lock();

        // rpm（固定分钟窗口）
        if client.rate_limit.rpm > 0 {
            let e = g.rpm.entry(client.id.clone()).or_insert((minute, 0));
            if e.0 != minute {
                *e = (minute, 0);
            }
            if e.1 >= client.rate_limit.rpm {
                let retry_after = 60 - ((now.timestamp() as u64) % 60);
                return Err(UsageDenied::RateLimited { retry_after });
            }
        }
        // daily 配额
        if client.quota.daily > 0 {
            let e = g.daily.entry(client.id.clone()).or_insert((dk.clone(), 0));
            if e.0 != dk {
                *e = (dk.clone(), 0);
            }
            if e.1 >= client.quota.daily {
                return Err(UsageDenied::QuotaExceeded { scope: "daily" });
            }
        }
        // monthly 配额
        if client.quota.monthly > 0 {
            let e = g
                .monthly
                .entry(client.id.clone())
                .or_insert((mk.clone(), 0));
            if e.0 != mk {
                *e = (mk.clone(), 0);
            }
            if e.1 >= client.quota.monthly {
                return Err(UsageDenied::QuotaExceeded { scope: "monthly" });
            }
        }
        // concurrency
        if client.rate_limit.concurrency > 0 {
            let c = g.concurrency.get(&client.id).copied().unwrap_or(0);
            if c >= client.rate_limit.concurrency {
                return Err(UsageDenied::Concurrency);
            }
        }

        // 提交计数
        g.rpm.entry(client.id.clone()).or_insert((minute, 0)).1 += 1;
        g.daily.entry(client.id.clone()).or_insert((dk, 0)).1 += 1;
        g.monthly.entry(client.id.clone()).or_insert((mk, 0)).1 += 1;
        *g.concurrency.entry(client.id.clone()).or_insert(0) += 1;

        Ok(ConcurrencyGuard {
            state: self.clone(),
            client_id: client.id.clone(),
        })
    }

    /// 返回某调用方当前用量快照（供管理面展示）。
    pub fn snapshot(&self, client_id: &str) -> Value {
        let g = self.inner.lock();
        serde_json::json!({
            "rpm_current": g.rpm.get(client_id).map(|(_, c)| *c).unwrap_or(0),
            "concurrency_current": g.concurrency.get(client_id).copied().unwrap_or(0),
            "daily_used": g.daily.get(client_id).map(|(_, c)| *c).unwrap_or(0),
            "monthly_used": g.monthly.get(client_id).map(|(_, c)| *c).unwrap_or(0),
        })
    }
}
