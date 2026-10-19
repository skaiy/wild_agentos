//! gRPC LLM gateway override validation (#450 / #299).

use tonic::Status;

use crate::api::http::config::same_provider_endpoint;
use crate::config::settings::Settings;
use crate::gateway::unified_gateway::UnifiedGateway;

pub(crate) const EXPLICIT_API_KEY_REQUIRED: &str = "explicit_api_key_required";

/// Reject a caller-supplied `llm_base_url` that would send the server's
/// configured gateway API key to a different endpoint.
#[allow(clippy::result_large_err)]
pub(crate) fn validate_grpc_llm_gateway_override(
    llm_base_url: &str,
    llm_api_key: &str,
    settings: &Settings,
    gateway: &UnifiedGateway,
) -> Result<(), Status> {
    let new_base = llm_base_url.trim();
    if new_base.is_empty() {
        return Ok(());
    }
    let explicit_key = !llm_api_key.trim().is_empty();
    if explicit_key {
        return Ok(());
    }
    if same_provider_endpoint(new_base, &settings.gateway.base_url) {
        return Ok(());
    }
    if gateway.api_key_configured() {
        return Err(Status::invalid_argument(EXPLICIT_API_KEY_REQUIRED));
    }
    // No server key to leak; still refuse an unauthenticated override host.
    Err(Status::invalid_argument(EXPLICIT_API_KEY_REQUIRED))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::settings::Settings;
    use crate::gateway::unified_gateway::UnifiedGateway;

    fn gateway_with_key() -> UnifiedGateway {
        let mut settings = Settings::default();
        settings.gateway.base_url = "https://deploy.invalid/v1".into();
        settings.gateway.api_key = "saved-key".into();
        UnifiedGateway::new(&settings.gateway).expect("gateway")
    }

    #[test]
    fn empty_override_is_allowed() {
        let settings = Settings::default();
        let gateway = UnifiedGateway::new(&settings.gateway).expect("gateway");
        assert!(validate_grpc_llm_gateway_override("", "", &settings, &gateway).is_ok());
    }

    #[test]
    fn same_endpoint_without_key_is_allowed() {
        let mut settings = Settings::default();
        settings.gateway.base_url = "https://deploy.invalid/v1".into();
        settings.gateway.api_key = "saved-key".into();
        let gateway = UnifiedGateway::new(&settings.gateway).expect("gateway");
        assert!(validate_grpc_llm_gateway_override(
            "https://deploy.invalid/v1/",
            "",
            &settings,
            &gateway,
        )
        .is_ok());
    }

    #[test]
    fn foreign_endpoint_without_key_is_rejected() {
        let mut settings = Settings::default();
        settings.gateway.base_url = "https://deploy.invalid/v1".into();
        settings.gateway.api_key = "saved-key".into();
        let gateway = gateway_with_key();
        let err = validate_grpc_llm_gateway_override(
            "https://attacker.invalid/v1",
            "",
            &settings,
            &gateway,
        )
        .expect_err("must reject");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert_eq!(err.message(), EXPLICIT_API_KEY_REQUIRED);
    }

    #[test]
    fn foreign_endpoint_with_explicit_key_is_allowed() {
        let mut settings = Settings::default();
        settings.gateway.base_url = "https://deploy.invalid/v1".into();
        settings.gateway.api_key = "saved-key".into();
        let gateway = gateway_with_key();
        assert!(validate_grpc_llm_gateway_override(
            "https://attacker.invalid/v1",
            "caller-key",
            &settings,
            &gateway,
        )
        .is_ok());
    }
}
