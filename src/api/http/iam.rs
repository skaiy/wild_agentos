/*!
 * IAM 身份提取中间件（G7）。
 *
 * 优先级：
 *   1. Authorization: Bearer <JWT>        —— HS256（开发）或 OIDC/JWKS 验证
 *   2. X-Identity: base64(JSON)            —— 开发/测试模拟身份
 *   3. 匿名（user_id="anonymous"）         —— 无凭据回退
 *
 * AGENTOS_AUTH_MODE=hs256（默认）使用 AGENTOS_JWT_SECRET；生产环境应使用
 * AGENTOS_AUTH_MODE=oidc 和 OIDC issuer/audience/JWKS 配置。
 * AGENTOS_AUTH_STRICT=true 强制执行角色校验，并拒绝 X-Identity（默认 false，
 * 适合本地开发）。严格模式中，JWT 是唯一可用的 HTTP 身份来源。
 */

use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::isolation::IsolationClaims;

// ─── JWT Claims ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    /// Subject = user_id
    pub sub: String,
    pub tenant_id: String,
    /// Project scope for isolation. Legacy tokens without this claim use
    /// the `default` project.
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    /// Unix timestamp 过期时间。
    pub exp: usize,
}

// ─── UserIdentity ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum AuthMethod {
    Jwt,
    Base64Header,
    ApiKey,
    Anonymous,
}

#[derive(Debug, Clone)]
pub struct UserIdentity {
    pub user_id: String,
    pub tenant_id: String,
    pub roles: Vec<String>,
    pub auth_method: AuthMethod,
    isolation_claims: Option<IsolationClaims>,
}

impl UserIdentity {
    pub fn anonymous() -> Self {
        Self {
            user_id: "anonymous".to_string(),
            tenant_id: "default".to_string(),
            roles: vec![],
            auth_method: AuthMethod::Anonymous,
            isolation_claims: None,
        }
    }
    /// Returns claims only when the authentication boundary verified them.
    pub fn isolation_claims(&self) -> Option<&IsolationClaims> {
        self.isolation_claims.as_ref()
    }
    /// 检查调用方是否具有指定角色（任一匹配）。
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r.as_str() == role)
    }
    /// 在严格模式下检查角色；非严格模式下匿名用户放行（便于本地开发）。
    pub fn require_role(&self, role: &str) -> Result<(), (StatusCode, Json<Value>)> {
        if self.has_role(role) {
            return Ok(());
        }
        if !auth_strict() && self.auth_method == AuthMethod::Anonymous {
            return Ok(());
        }
        Err((
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "forbidden",
                "required_role": role,
                "user_id": self.user_id,
                "user_roles": self.roles,
                "hint": "Set AGENTOS_AUTH_STRICT=false to bypass role checks in dev mode",
            })),
        ))
    }
}

// ─── Axum Extractor ───────────────────────────────────────────────────────────

#[async_trait]
impl<S: Send + Sync> FromRequestParts<S> for UserIdentity {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // 1. JWT Bearer
        if let Some(auth) = parts.headers.get("authorization") {
            if let Ok(val) = auth.to_str() {
                if let Some(token) = val.strip_prefix("Bearer ") {
                    if let Some(identity) = verify_jwt(token).await {
                        return Ok(identity);
                    }
                }
            }
        }
        // 2. X-Identity base64-JSON（开发模拟）
        if let Some(hdr) = parts.headers.get("x-identity") {
            if auth_strict() {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    "X-Identity is disabled when AGENTOS_AUTH_STRICT=true".to_string(),
                ));
            }
            if let Ok(val) = hdr.to_str() {
                if let Ok(bytes) = STANDARD.decode(val) {
                    if let Ok(claims) = serde_json::from_slice::<Value>(&bytes) {
                        return Ok(UserIdentity {
                            user_id: str_field(&claims, "user_id", "anonymous"),
                            tenant_id: str_field(&claims, "tenant_id", "default"),
                            roles: arr_field(&claims, "roles"),
                            auth_method: AuthMethod::Base64Header,
                            isolation_claims: None,
                        });
                    }
                }
            }
        }
        // 3. Anonymous fallback
        Ok(UserIdentity::anonymous())
    }
}

// ─── JWT 验签 ─────────────────────────────────────────────────────────────────

fn auth_strict() -> bool {
    std::env::var("AGENTOS_AUTH_STRICT").as_deref() == Ok("true")
}

fn jwt_secret() -> String {
    std::env::var("AGENTOS_JWT_SECRET")
        .unwrap_or_else(|_| "agentos-dev-secret-change-in-prod".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    Hs256,
    Oidc,
}

fn auth_mode() -> Result<AuthMode, String> {
    match std::env::var("AGENTOS_AUTH_MODE")
        .unwrap_or_else(|_| "hs256".to_string())
        .as_str()
    {
        "hs256" => Ok(AuthMode::Hs256),
        "oidc" => Ok(AuthMode::Oidc),
        value => Err(format!("unsupported AGENTOS_AUTH_MODE {value:?}")),
    }
}

#[derive(Debug, Clone)]
struct OidcConfig {
    jwks_url: String,
    issuer: String,
    audience: String,
}

fn required_env(name: &str) -> Result<String, String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} must be configured for OIDC authentication"))
}

fn oidc_config() -> Result<OidcConfig, String> {
    let jwks_url = required_env("AGENTOS_OIDC_JWKS_URL")?;
    let loopback_http = jwks_url.starts_with("http://127.0.0.1")
        || jwks_url.starts_with("http://localhost")
        || jwks_url.starts_with("http://[::1]");
    if !jwks_url.starts_with("https://") && !loopback_http {
        return Err(
            "AGENTOS_OIDC_JWKS_URL must use HTTPS (HTTP is limited to loopback development)"
                .to_string(),
        );
    }
    Ok(OidcConfig {
        jwks_url,
        issuer: required_env("AGENTOS_OIDC_ISSUER")?,
        audience: required_env("AGENTOS_OIDC_AUDIENCE")?,
    })
}

struct CachedJwks {
    url: String,
    fetched_at: Instant,
    keys: Arc<JwkSet>,
}

static JWKS_CACHE: Lazy<Mutex<Option<CachedJwks>>> = Lazy::new(|| Mutex::new(None));
const JWKS_CACHE_TTL: Duration = Duration::from_secs(300);

async fn jwks_for(config: &OidcConfig, refresh: bool) -> Option<Arc<JwkSet>> {
    if !refresh {
        let cache = JWKS_CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cached) = cache.as_ref().filter(|cached| {
            cached.url == config.jwks_url && cached.fetched_at.elapsed() < JWKS_CACHE_TTL
        }) {
            return Some(Arc::clone(&cached.keys));
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .ok()?;
    let keys = client
        .get(&config.jwks_url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json::<JwkSet>()
        .await
        .ok()?;
    let keys = Arc::new(keys);
    *JWKS_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some(CachedJwks {
        url: config.jwks_url.clone(),
        fetched_at: Instant::now(),
        keys: Arc::clone(&keys),
    });
    Some(keys)
}

fn claims_identity(claims: JwtClaims) -> Option<UserIdentity> {
    let project_id = claims
        .project_id
        .as_deref()
        .filter(|project_id| !project_id.is_empty())
        .unwrap_or("default");
    let isolation_claims =
        IsolationClaims::from_verified(claims.tenant_id.clone(), project_id, claims.sub.clone())
            .ok()?;
    Some(UserIdentity {
        user_id: claims.sub,
        tenant_id: claims.tenant_id,
        roles: claims.roles,
        auth_method: AuthMethod::Jwt,
        isolation_claims: Some(isolation_claims),
    })
}

async fn verify_jwt(token: &str) -> Option<UserIdentity> {
    match auth_mode() {
        Ok(AuthMode::Hs256) => verify_hs256_jwt(token),
        Ok(AuthMode::Oidc) => verify_oidc_jwt(token).await,
        Err(error) => {
            tracing::warn!("JWT authentication configuration rejected: {}", error);
            None
        }
    }
}

fn verify_hs256_jwt(token: &str) -> Option<UserIdentity> {
    let key = DecodingKey::from_secret(jwt_secret().as_bytes());
    let mut val = Validation::new(Algorithm::HS256);
    val.set_required_spec_claims(&["sub", "exp"]);
    match decode::<JwtClaims>(token, &key, &val) {
        Ok(data) => claims_identity(data.claims),
        Err(e) => {
            tracing::debug!("HS256 JWT verification failed: {}", e);
            None
        }
    }
}

async fn verify_oidc_jwt(token: &str) -> Option<UserIdentity> {
    let config = oidc_config()
        .map_err(|error| tracing::warn!("{}", error))
        .ok()?;
    let header = decode_header(token)
        .map_err(|error| tracing::debug!("OIDC JWT header decode failed: {}", error))
        .ok()?;
    if !matches!(
        header.alg,
        Algorithm::RS256
            | Algorithm::RS384
            | Algorithm::RS512
            | Algorithm::ES256
            | Algorithm::ES384
            | Algorithm::EdDSA
    ) {
        tracing::warn!(algorithm = ?header.alg, "OIDC JWT used a disallowed asymmetric algorithm");
        return None;
    }
    let kid = header
        .kid
        .as_deref()
        .filter(|kid| !kid.is_empty())
        .or_else(|| {
            tracing::debug!("OIDC JWT has no kid");
            None
        })?;
    let keys = jwks_for(&config, false).await?;
    let keys = if keys.find(kid).is_some() {
        keys
    } else {
        // A key rotation may have happened since the cache was populated.
        jwks_for(&config, true).await?
    };
    let jwk = keys.find(kid)?;
    let decoding_key = DecodingKey::from_jwk(jwk)
        .map_err(|error| tracing::debug!("OIDC JWK decode failed: {}", error))
        .ok()?;
    let mut validation = Validation::new(header.alg);
    validation.set_required_spec_claims(&["sub", "exp", "iss", "aud"]);
    validation.set_issuer(&[config.issuer.as_str()]);
    validation.set_audience(&[config.audience.as_str()]);
    match decode::<JwtClaims>(token, &decoding_key, &validation) {
        Ok(data) => claims_identity(data.claims),
        Err(error) => {
            tracing::debug!("OIDC JWT verification failed: {}", error);
            None
        }
    }
}

// ─── Helper ───────────────────────────────────────────────────────────────────

fn str_field(v: &Value, key: &str, default: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or(default)
        .to_string()
}
fn arr_field(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|r| r.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use axum::{
        extract::FromRequestParts,
        http::{Request, StatusCode},
    };
    use base64::{engine::general_purpose::STANDARD, Engine};
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    use serde::Serialize;

    use super::{verify_jwt, AuthMethod, JwtClaims, UserIdentity};
    use crate::api::http::TEST_ENV_LOCK;

    #[tokio::test]
    async fn strict_mode_rejects_forged_x_identity_before_claims_are_created() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("AGENTOS_AUTH_STRICT");
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");
        let forged = STANDARD.encode(r#"{"tenant_id":"evil","roles":["DA"]}"#);
        let (mut parts, _) = Request::builder()
            .header("x-identity", forged)
            .body(())
            .unwrap()
            .into_parts();

        let rejection = UserIdentity::from_request_parts(&mut parts, &())
            .await
            .unwrap_err();
        assert_eq!(rejection.0, StatusCode::UNAUTHORIZED);

        restore_strict_mode(previous);
    }

    #[tokio::test]
    async fn strict_mode_does_not_read_tenant_identity_from_request_body() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("AGENTOS_AUTH_STRICT");
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");
        let (mut parts, _) = Request::builder()
            .body(r#"{"tenant":"evil","tenant_id":"also-evil","roles":["DA"]}"#)
            .unwrap()
            .into_parts();

        let identity = UserIdentity::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(identity.auth_method, AuthMethod::Anonymous);
        assert_ne!(identity.tenant_id, "evil");
        assert!(
            identity.isolation_claims().is_none(),
            "an unverified request body must not mint isolation claims"
        );
        assert!(identity.require_role("DA").is_err());

        restore_strict_mode(previous);
    }

    #[tokio::test]
    async fn isolation_contract_x_identity_never_creates_isolation_claims() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("AGENTOS_AUTH_STRICT");
        std::env::remove_var("AGENTOS_AUTH_STRICT");
        let simulated = STANDARD.encode(r#"{"user_id":"developer","tenant_id":"dev-tenant"}"#);
        let (mut parts, _) = Request::builder()
            .header("x-identity", simulated)
            .body(())
            .unwrap()
            .into_parts();

        let identity = UserIdentity::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(identity.auth_method, AuthMethod::Base64Header);
        assert_eq!(identity.tenant_id, "dev-tenant");
        assert!(
            identity.isolation_claims().is_none(),
            "X-Identity is a development simulation, not a trusted claims source"
        );
        restore_strict_mode(previous);
    }

    #[tokio::test]
    async fn strict_mode_accepts_a_valid_jwt_identity() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os("AGENTOS_AUTH_STRICT");
        let previous_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        let token = encode(
            &Header::default(),
            &JwtClaims {
                sub: "service".to_string(),
                tenant_id: "acme".to_string(),
                project_id: None,
                roles: vec!["DA".to_string()],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"agentos-dev-secret-change-in-prod"),
        )
        .unwrap();
        let (mut parts, _) = Request::builder()
            .header("authorization", format!("Bearer {token}"))
            .body(())
            .unwrap()
            .into_parts();

        let identity = UserIdentity::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        assert_eq!(identity.auth_method, AuthMethod::Jwt);
        assert_eq!(identity.tenant_id, "acme");
        assert_eq!(
            identity.isolation_claims().unwrap().graph_iri().unwrap(),
            "graph://acme/default"
        );
        assert!(identity.require_role("DA").is_ok());

        restore_strict_mode(previous);
        restore_env("AGENTOS_AUTH_MODE", previous_mode);
    }

    #[tokio::test]
    async fn legacy_jwt_without_project_claim_uses_default_project() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        #[derive(Serialize)]
        struct LegacyJwtClaims {
            sub: String,
            tenant_id: String,
            exp: usize,
        }

        let token = encode(
            &Header::default(),
            &LegacyJwtClaims {
                sub: "service".to_string(),
                tenant_id: "acme".to_string(),
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"agentos-dev-secret-change-in-prod"),
        )
        .unwrap();

        let identity = verify_jwt(&token).await.unwrap();
        let claims = identity.isolation_claims().unwrap();
        assert_eq!(claims.project_id(), "default");
        assert_eq!(claims.graph_iri().unwrap(), "graph://acme/default");
        assert_eq!(claims.vector_namespace().unwrap(), "vector://acme/default");
        restore_env("AGENTOS_AUTH_MODE", previous_mode);
    }

    #[tokio::test]
    async fn jwt_with_empty_project_claim_uses_default_project() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        let token = encode(
            &Header::default(),
            &JwtClaims {
                sub: "service".to_string(),
                tenant_id: "acme".to_string(),
                project_id: Some(String::new()),
                roles: vec![],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"agentos-dev-secret-change-in-prod"),
        )
        .unwrap();

        let claims = verify_jwt(&token)
            .await
            .unwrap()
            .isolation_claims()
            .unwrap()
            .clone();
        assert_eq!(claims.project_id(), "default");
        restore_env("AGENTOS_AUTH_MODE", previous_mode);
    }

    #[tokio::test]
    async fn jwt_project_claim_mints_project_scoped_names() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        let token = encode(
            &Header::default(),
            &JwtClaims {
                sub: "service".to_string(),
                tenant_id: "acme".to_string(),
                project_id: Some("research_1".to_string()),
                roles: vec![],
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_secret(b"agentos-dev-secret-change-in-prod"),
        )
        .unwrap();

        let claims = verify_jwt(&token)
            .await
            .unwrap()
            .isolation_claims()
            .unwrap()
            .clone();
        assert_eq!(claims.project_id(), "research_1");
        assert_eq!(claims.graph_iri().unwrap(), "graph://acme/research_1");
        assert_eq!(
            claims.vector_namespace().unwrap(),
            "vector://acme/research_1"
        );
        restore_env("AGENTOS_AUTH_MODE", previous_mode);
    }

    #[tokio::test]
    async fn unsafe_jwt_project_claims_fail_closed() {
        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous_mode = std::env::var_os("AGENTOS_AUTH_MODE");
        std::env::set_var("AGENTOS_AUTH_MODE", "hs256");
        for project_id in [".", "..", "a/b"] {
            let token = encode(
                &Header::default(),
                &JwtClaims {
                    sub: "service".to_string(),
                    tenant_id: "acme".to_string(),
                    project_id: Some(project_id.to_string()),
                    roles: vec![],
                    exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
                },
                &EncodingKey::from_secret(b"agentos-dev-secret-change-in-prod"),
            )
            .unwrap();

            assert!(
                verify_jwt(&token).await.is_none(),
                "{project_id:?} was accepted"
            );
        }
        restore_env("AGENTOS_AUTH_MODE", previous_mode);
    }

    #[derive(Serialize)]
    struct OidcJwtClaims {
        sub: String,
        tenant_id: String,
        project_id: String,
        roles: Vec<String>,
        iss: String,
        aud: String,
        exp: usize,
    }

    const TEST_RSA_PRIVATE_KEY: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAhURhZoOh6atOtKyK4W56CRODmWSVKPNA6zF96o9G/+WXpfeI
64BASV9IFnad820UY9eHeXmOP6zmJl/emcRBh5i5UKLWXVQ1NrvMBUpF7+HQU9Zr
ulbPsgnhMII1vLMAp6Wdfj+ejj0YzjSrx/peId0S2fOlJg64ENwUzRZm+w01ch2s
1myb5Vci3MPCPDMiygTBRH+ixZeuOjgQUJeTXzwvaHPJviXPFEtZ+72j4ZQ7lDtM
9sQqP9UT+HXTAgeWgWbtrK8bIhkWVPT3CGwQpi/YIc5OSDD0IP7HPBamQw7si4ia
saKypFMstSWwT3fJc0Pl1aPvAjrcPOFIigr2JwIDAQABAoIBACCqpNdsn8U37SiD
fN2KZ5aO9nykt51cl0avkIZtDYHPhQ81MJZNjzSNCw4akFgpnkxk+fvQTIqWNqok
aNu3TDrROGeoKrSg3hRnDzkivibxatAKKMj525pwKodp+4MgO6JcidD3BkYmesyd
A5iW6fkSCDttqkc8Z2kWkXC+M4sJFMU7OlcY8KWQSHtQI+lb2s+uiP0TW53lp4kV
Q8OMcZDzdeEWbLzqQ+wFtlCDOFjk1EXmi23vp+YZ1ssxbNP6pSEDRXnDO2FYOYqh
vF3UwfhNyj1+uPsQMyBdQ/de3SzBouFWm/0GdVi5DRyCvGU3WuW5CdHBbhIAV3Hm
S8pUR60CgYEAvHLeaYcBv1imwz/PYaOEFYhZaG3eh2AWtyUb8oJe0LwhSOViN5Fb
4UIv46JYOq3yqo1SiTT1SRyEKruCkqi4IsZ8OMkqvmGDdinrmHKvNosot0ZLH1ce
KQMa3QzdJftflz87FTB1ZtNIsqCtf8b71HrsT+1yVBaizhJ8FD9SP20CgYEAtQm7
ndH1bVFimSnfYN4KObuOcd8oS4KthcqW5VspiCeSFjDmJhnnpNB+4VB8zIT9At5O
zdY0XGmzVP7D8BDoG2E5V38me4PaBAowRtLZb5kTV6fv24+t3HxX38CZ7XpW95y0
9I8Kub8bWwEWCObT77nZ4CvB5fR0pZ1+14UVy2MCgYEAmRVfI65udvgXEAkn+BMS
20MWDkUiPiqKiWB14XySdVI+X68nKCjG0KgpqutYbOKdfHqtD5SbpTarDuOf4G96
lZVTl/Wi6WDhn/3RytdvCgnlm2xY3i6w63QAQI2QoKghMQZGgqII3OzJ44GvL1t/
e04X5Z3n//MbcfeGIBSIRckCgYAOkZvxlWXkyDnhDYeWagf0oW1TKJw7h2ajb6w5
BN8Qv+53rrO2uTr0/npXc3y3kLQzuOQqmGRaU39FBcOK3DFxkp9ktSzJn9C5poBA
EtPAsVbnJPKefq+FINSJgxxgCgpZntjJHYHFdOWkqy+0w66mihRIf/z4nnWMpmIA
wgsA9QKBgBMCSFjZWXNyoglccresoPzUcahcofydurIOHoaWzelJaafNiGDYXqW3
vX/Fd5UxB4QtKVYIN7dTj+xzNCeotUwPJCx22JnqC40gUiQ2qZtyF9LQTSZuATUQ
rOaa4PuObG218MVBl8eR9G5Ni7YF7jSktxKJi14QJr2E00x2h4Ih
-----END RSA PRIVATE KEY-----";
    const TEST_RSA_N: &str = "hURhZoOh6atOtKyK4W56CRODmWSVKPNA6zF96o9G_-WXpfeI64BASV9IFnad820UY9eHeXmOP6zmJl_emcRBh5i5UKLWXVQ1NrvMBUpF7-HQU9ZrulbPsgnhMII1vLMAp6Wdfj-ejj0YzjSrx_peId0S2fOlJg64ENwUzRZm-w01ch2s1myb5Vci3MPCPDMiygTBRH-ixZeuOjgQUJeTXzwvaHPJviXPFEtZ-72j4ZQ7lDtM9sQqP9UT-HXTAgeWgWbtrK8bIhkWVPT3CGwQpi_YIc5OSDD0IP7HPBamQw7si4iasaKypFMstSWwT3fJc0Pl1aPvAjrcPOFIigr2Jw";

    #[tokio::test]
    async fn oidc_jwks_verifies_claims_and_rejects_wrong_issuer() {
        use axum::{routing::get, Json, Router};
        use serde_json::json;

        let _guard = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let jwks = json!({"keys": [{
            "kty": "RSA", "kid": "test-rsa", "use": "sig", "alg": "RS256",
            "n": TEST_RSA_N, "e": "AQAB"
        }]});
        let app = Router::new().route(
            "/jwks",
            get(move || {
                let jwks = jwks.clone();
                async move { Json(jwks) }
            }),
        );
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let saved: Vec<_> = [
            "AGENTOS_AUTH_MODE",
            "AGENTOS_OIDC_JWKS_URL",
            "AGENTOS_OIDC_ISSUER",
            "AGENTOS_OIDC_AUDIENCE",
        ]
        .into_iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
        std::env::set_var("AGENTOS_AUTH_MODE", "oidc");
        std::env::set_var("AGENTOS_OIDC_JWKS_URL", format!("http://{address}/jwks"));
        std::env::set_var("AGENTOS_OIDC_ISSUER", "https://issuer.example.test");
        std::env::set_var("AGENTOS_OIDC_AUDIENCE", "wild-agent-os");

        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("test-rsa".to_string());
        let token = encode(
            &header,
            &OidcJwtClaims {
                sub: "service".to_string(),
                tenant_id: "acme".to_string(),
                project_id: "research_1".to_string(),
                roles: vec!["DA".to_string()],
                iss: "https://issuer.example.test".to_string(),
                aud: "wild-agent-os".to_string(),
                exp: (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
            },
            &EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap(),
        )
        .unwrap();
        let identity = verify_jwt(&token).await.expect("valid OIDC token");
        assert_eq!(identity.tenant_id, "acme");
        assert_eq!(
            identity.isolation_claims().unwrap().project_id(),
            "research_1"
        );

        std::env::set_var("AGENTOS_OIDC_ISSUER", "https://other-issuer.example.test");
        assert!(
            verify_jwt(&token).await.is_none(),
            "an invalid OIDC issuer must not mint claims"
        );

        server.abort();
        for (name, value) in saved {
            restore_env(name, value);
        }
    }

    fn restore_strict_mode(previous: Option<std::ffi::OsString>) {
        restore_env("AGENTOS_AUTH_STRICT", previous);
    }

    fn restore_env(name: &str, previous: Option<std::ffi::OsString>) {
        if let Some(value) = previous {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }
}
