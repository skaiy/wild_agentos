//! A2A v1.0 HTTP+JSON outbound client.
//!
//! The minimal message set is intentionally limited to `SendMessage`:
//! a local task request and its terminal result. See `docs/19-a2a-outbound.md`.

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};
use thiserror::Error;
use uuid::Uuid;

use crate::{config::settings::A2aOutboundSettings, isolation::IsolationClaims};

pub const A2A_PROTOCOL_VERSION: &str = "1.0";

#[derive(Debug, Error)]
pub enum A2aOutboundError {
    #[error("outbound A2A is disabled")]
    Disabled,
    #[error("outbound A2A endpoint is not configured")]
    MissingEndpoint,
    #[error("outbound A2A request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("outbound A2A endpoint returned {status}: {body}")]
    Http {
        status: reqwest::StatusCode,
        body: String,
    },
}

/// A narrowly scoped HTTP+JSON client for an already-configured remote A2A
/// endpoint. It never forwards the caller's bearer token; optional remote
/// credentials come only from this adapter's service configuration.
#[derive(Clone)]
pub struct A2aOutboundClient {
    enabled: bool,
    endpoint: String,
    bearer_token: String,
    client: Client,
}

impl A2aOutboundClient {
    pub fn from_settings(settings: &A2aOutboundSettings) -> Result<Self, A2aOutboundError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(settings.timeout_seconds))
            .build()?;
        Ok(Self {
            enabled: settings.enabled,
            endpoint: settings.endpoint.trim_end_matches('/').to_string(),
            bearer_token: settings.bearer_token.clone(),
            client,
        })
    }

    /// Send the local user request and return the remote task id, when the
    /// remote agent chose to create a task rather than return a direct message.
    pub async fn send_task_request(
        &self,
        task_iri: &str,
        prompt: &str,
        claims: Option<&IsolationClaims>,
    ) -> Result<Option<String>, A2aOutboundError> {
        self.send_message(
            task_iri,
            prompt,
            "request",
            None,
            claims,
            Some(json!({
                "returnImmediately": true,
                "acceptedOutputModes": ["text/plain"],
            })),
        )
        .await
    }

    /// Send the local task's terminal result as a follow-up, associating it
    /// with the remote task when the initial request returned an id.
    pub async fn send_task_result(
        &self,
        task_iri: &str,
        remote_task_id: Option<&str>,
        status: &str,
        summary: &str,
        output: Option<&Value>,
        claims: Option<&IsolationClaims>,
    ) -> Result<(), A2aOutboundError> {
        let mut text = format!("Local task completed with status `{status}`.\n\n{summary}");
        if let Some(output) = output {
            text.push_str("\n\nOutput:\n");
            text.push_str(&output.to_string());
        }
        self.send_message(task_iri, &text, "result", remote_task_id, claims, None)
            .await
            .map(|_| ())
    }

    async fn send_message(
        &self,
        task_iri: &str,
        text: &str,
        event: &str,
        remote_task_id: Option<&str>,
        claims: Option<&IsolationClaims>,
        configuration: Option<Value>,
    ) -> Result<Option<String>, A2aOutboundError> {
        if !self.enabled {
            return Err(A2aOutboundError::Disabled);
        }
        if self.endpoint.is_empty() {
            return Err(A2aOutboundError::MissingEndpoint);
        }

        let mut message = json!({
            "role": "ROLE_USER",
            "parts": [{"text": text}],
            "messageId": Uuid::new_v4().to_string(),
        });
        if let Some(remote_task_id) = remote_task_id {
            message["taskId"] = json!(remote_task_id);
        }
        let mut metadata = json!({
            "wildAgentOs": {
                "taskIri": task_iri,
                "event": event,
            }
        });
        if let Some(claims) = claims {
            // Claims are copied as attributed context, never as a bearer token.
            metadata["wildAgentOs"]["claims"] = json!({
                "tenantId": claims.tenant_id(),
                "projectId": claims.project_id(),
                "actorId": claims.actor_id(),
            });
        }
        let mut body = json!({"message": message, "metadata": metadata});
        if let Some(configuration) = configuration {
            body["configuration"] = configuration;
        }

        let endpoint = if self.endpoint.ends_with("/message:send") {
            self.endpoint.clone()
        } else {
            format!("{}/message:send", self.endpoint)
        };
        let mut request = self
            .client
            .post(endpoint)
            .header("Content-Type", "application/a2a+json")
            .header("Accept", "application/a2a+json")
            .header("A2A-Version", A2A_PROTOCOL_VERSION)
            .json(&body);
        if !self.bearer_token.is_empty() {
            request = request.bearer_auth(&self.bearer_token);
        }

        let response = request.send().await?;
        let status = response.status();
        let response_body = response.text().await?;
        if !status.is_success() {
            return Err(A2aOutboundError::Http {
                status,
                body: response_body.chars().take(512).collect(),
            });
        }
        let response_json: Value = serde_json::from_str(&response_body).unwrap_or(Value::Null);
        Ok(response_json
            .pointer("/task/id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::post,
        Json, Router,
    };
    use tokio::sync::Mutex;

    use super::*;

    fn enabled_settings(endpoint: String) -> A2aOutboundSettings {
        A2aOutboundSettings {
            enabled: true,
            endpoint,
            bearer_token: "remote-service-token".to_string(),
            timeout_seconds: 2,
        }
    }

    #[tokio::test]
    async fn outbound_request_and_result_use_a2a_http_json() {
        let received = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
        let app = Router::new()
            .route(
                "/message:send",
                post(
                    |State(received): State<Arc<Mutex<Vec<(HeaderMap, Value)>>>>,
                     headers: HeaderMap,
                     Json(body): Json<Value>| async move {
                        received.lock().await.push((headers, body));
                        Json(json!({"task": {"id": "remote-task-1"}}))
                    },
                ),
            )
            .with_state(received.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app));

        let client =
            A2aOutboundClient::from_settings(&enabled_settings(format!("http://{address}")))
                .unwrap();
        let claims = IsolationClaims::from_verified("tenant-a", "project-a", "actor-a").unwrap();
        let remote_id = client
            .send_task_request("iri://task/local-1", "summarize this", Some(&claims))
            .await
            .unwrap();
        client
            .send_task_result(
                "iri://task/local-1",
                remote_id.as_deref(),
                "success",
                "done",
                Some(&json!({"answer": "42"})),
                Some(&claims),
            )
            .await
            .unwrap();

        let received = received.lock().await;
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].0["a2a-version"], "1.0");
        assert_eq!(
            received[0].0["authorization"],
            "Bearer remote-service-token"
        );
        assert_eq!(received[0].1["message"]["role"], "ROLE_USER");
        assert_eq!(
            received[0].1["metadata"]["wildAgentOs"]["claims"]["tenantId"],
            "tenant-a"
        );
        assert_eq!(received[1].1["message"]["taskId"], "remote-task-1");
        assert_eq!(received[1].1["metadata"]["wildAgentOs"]["event"], "result");
    }

    #[tokio::test]
    async fn outbound_failure_is_reported_without_response_body_leak() {
        let app = Router::new().route(
            "/message:send",
            post(|| async { (StatusCode::BAD_GATEWAY, "remote unavailable") }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, app));

        let client =
            A2aOutboundClient::from_settings(&enabled_settings(format!("http://{address}")))
                .unwrap();
        let error = client
            .send_task_request("iri://task/local-1", "summarize this", None)
            .await
            .unwrap_err();
        assert!(
            matches!(error, A2aOutboundError::Http { status, .. } if status == StatusCode::BAD_GATEWAY)
        );
    }
}
