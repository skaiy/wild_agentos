/*!
 * IAM 身份提取中间件（G7）。
 *
 * 优先级：
 *   1. Authorization: Bearer <HS256 JWT>  —— 生产级签名验证
 *   2. X-Identity: base64(JSON)            —— 开发/测试模拟身份
 *   3. 匿名（user_id="anonymous"）         —— 无凭据回退
 *
 * AGENTOS_JWT_SECRET 环境变量控制签名密钥。
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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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
                    if let Some(identity) = verify_jwt(token) {
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

fn verify_jwt(token: &str) -> Option<UserIdentity> {
    use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
    let key = DecodingKey::from_secret(jwt_secret().as_bytes());
    let mut val = Validation::new(Algorithm::HS256);
    val.set_required_spec_claims(&["sub", "exp"]);
    match decode::<JwtClaims>(token, &key, &val) {
        Ok(data) => {
            let project_id = data
                .claims
                .project_id
                .as_deref()
                .filter(|project_id| !project_id.is_empty())
                .unwrap_or("default");
            let isolation_claims = IsolationClaims::from_verified(
                data.claims.tenant_id.clone(),
                project_id,
                data.claims.sub.clone(),
            )
            .ok()?;
            Some(UserIdentity {
                user_id: data.claims.sub,
                tenant_id: data.claims.tenant_id,
                roles: data.claims.roles,
                auth_method: AuthMethod::Jwt,
                isolation_claims: Some(isolation_claims),
            })
        }
        Err(e) => {
            tracing::debug!("JWT verify failed: {}", e);
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
    use jsonwebtoken::{encode, EncodingKey, Header};
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
        std::env::set_var("AGENTOS_AUTH_STRICT", "true");
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
    }

    #[test]
    fn legacy_jwt_without_project_claim_uses_default_project() {
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

        let identity = verify_jwt(&token).unwrap();
        let claims = identity.isolation_claims().unwrap();
        assert_eq!(claims.project_id(), "default");
        assert_eq!(claims.graph_iri().unwrap(), "graph://acme/default");
        assert_eq!(claims.vector_namespace().unwrap(), "vector://acme/default");
    }

    #[test]
    fn jwt_with_empty_project_claim_uses_default_project() {
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
            .unwrap()
            .isolation_claims()
            .unwrap()
            .clone();
        assert_eq!(claims.project_id(), "default");
    }

    #[test]
    fn jwt_project_claim_mints_project_scoped_names() {
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
    }

    #[test]
    fn unsafe_jwt_project_claims_fail_closed() {
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

            assert!(verify_jwt(&token).is_none(), "{project_id:?} was accepted");
        }
    }

    fn restore_strict_mode(previous: Option<std::ffi::OsString>) {
        if let Some(value) = previous {
            std::env::set_var("AGENTOS_AUTH_STRICT", value);
        } else {
            std::env::remove_var("AGENTOS_AUTH_STRICT");
        }
    }
}
