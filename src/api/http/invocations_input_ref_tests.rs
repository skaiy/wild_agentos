//! `input_ref` extension point + built-in artifact resolver tests.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use super::*;
use crate::api::http::artifacts::{
    artifact_key_for_tests, write_metadata_for_tests, ArtifactKind, ArtifactMetadata,
};
use crate::api::http::invocations_execution::prompt_from_invocation;
use crate::api::http::invocations_store::Invocation;
use crate::blob::LocalFsBlobStore;

/// Returns fixed bytes and records the claims and URI it was called with.
struct RecordingResolver {
    body: Vec<u8>,
    seen: Mutex<Vec<(String, String, String, String)>>,
}

impl RecordingResolver {
    fn new(body: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            body: body.to_vec(),
            seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl InputRefResolver for RecordingResolver {
    async fn resolve(&self, request: InputRefRequest<'_>) -> Result<Vec<u8>, InputRefError> {
        self.seen.lock().unwrap().push((
            request.claims.tenant_id().to_string(),
            request.claims.project_id().to_string(),
            request.claims.actor_id().to_string(),
            request.uri.to_string(),
        ));
        Ok(self.body.clone())
    }
}

struct FailingResolver(InputRefError);

#[async_trait]
impl InputRefResolver for FailingResolver {
    async fn resolve(&self, _request: InputRefRequest<'_>) -> Result<Vec<u8>, InputRefError> {
        Err(self.0)
    }
}

fn claims(tenant: &str, project: &str, actor: &str) -> IsolationClaims {
    IsolationClaims::from_verified(tenant, project, actor).unwrap()
}

#[tokio::test]
async fn longest_prefix_wins_then_scheme_then_nothing() {
    let registry = InputRefRegistry::new();
    let by_scheme = RecordingResolver::new(b"scheme");
    let short = RecordingResolver::new(b"short");
    let long = RecordingResolver::new(b"long");
    registry.register_scheme("s3", by_scheme.clone()).unwrap();
    registry
        .register_prefix("s3://bucket/", short.clone())
        .unwrap();
    registry
        .register_prefix("s3://bucket/tenant-a/", long.clone())
        .unwrap();
    let alice = claims("tenant-a", "project-a", "alice");

    assert_eq!(
        registry
            .resolve(&alice, "s3://bucket/tenant-a/k")
            .await
            .unwrap(),
        b"long"
    );
    assert_eq!(
        registry.resolve(&alice, "s3://bucket/other").await.unwrap(),
        b"short"
    );
    assert_eq!(
        registry.resolve(&alice, "s3://elsewhere/k").await.unwrap(),
        b"scheme"
    );
    assert_eq!(
        registry.resolve(&alice, "kb://x").await.unwrap_err(),
        InputRefError::NoResolver
    );
    assert!(registry.has_resolver_for("s3://elsewhere/k"));
    assert!(!registry.has_resolver_for("kb://x"));
    assert!(!registry.has_resolver_for("no-scheme"));
    assert_eq!(
        registry.prefixes(),
        vec![
            "s3://bucket/tenant-a/".to_string(),
            "s3://bucket/".to_string()
        ]
    );
}

#[tokio::test]
async fn prefix_only_registration_does_not_claim_the_whole_scheme() {
    let registry = InputRefRegistry::new();
    registry
        .register_prefix("s3://allowed/", RecordingResolver::new(b"x"))
        .unwrap();
    assert!(registry.has_resolver_for("s3://allowed/key"));
    assert!(!registry.has_resolver_for("s3://other/key"));
    assert!(!registry.has_resolver_for("s3://allowe"));
}

#[test]
fn registration_is_validated_and_first_come() {
    let registry = InputRefRegistry::new();
    for bad in ["", "S3", "1abc", "a b", "s3:", "wao_artifact"] {
        assert_eq!(
            registry.register_scheme(bad, RecordingResolver::new(b"")),
            Err(InputRefRegistrationError::InvalidScheme),
            "{bad:?}"
        );
    }
    for bad in ["s3", "://x", "S3://x", "s3://", "a b://x"] {
        assert_eq!(
            registry.register_prefix(bad, RecordingResolver::new(b"")),
            Err(InputRefRegistrationError::InvalidPrefix),
            "{bad:?}"
        );
    }
    registry
        .register_scheme("wao-artifact", RecordingResolver::new(b"builtin"))
        .unwrap();
    assert_eq!(
        registry.register_scheme("wao-artifact", RecordingResolver::new(b"evil")),
        Err(InputRefRegistrationError::Duplicate)
    );
    registry
        .register_prefix("s3://b/", RecordingResolver::new(b""))
        .unwrap();
    assert_eq!(
        registry.register_prefix("s3://b/", RecordingResolver::new(b"")),
        Err(InputRefRegistrationError::Duplicate)
    );
    assert_eq!(registry.schemes(), vec!["wao-artifact".to_string()]);
}

#[tokio::test]
async fn extend_from_skips_clashes_so_builtin_is_not_replaced() {
    let runtime = InputRefRegistry::new();
    runtime
        .register_scheme("wao-artifact", RecordingResolver::new(b"builtin"))
        .unwrap();
    let embedder = InputRefRegistry::new();
    embedder
        .register_scheme("wao-artifact", RecordingResolver::new(b"evil"))
        .unwrap();
    embedder
        .register_scheme("acme", RecordingResolver::new(b"acme"))
        .unwrap();
    embedder
        .register_prefix("s3://acme/", RecordingResolver::new(b"s3"))
        .unwrap();

    let skipped = runtime.extend_from(&embedder);
    assert_eq!(skipped, vec!["wao-artifact".to_string()]);
    let alice = claims("tenant-a", "project-a", "alice");
    assert_eq!(
        runtime.resolve(&alice, "wao-artifact://x").await.unwrap(),
        b"builtin"
    );
    assert_eq!(runtime.resolve(&alice, "acme://x").await.unwrap(), b"acme");
    assert_eq!(runtime.resolve(&alice, "s3://acme/k").await.unwrap(), b"s3");
}

#[tokio::test]
async fn resolver_always_receives_caller_claims() {
    let registry = InputRefRegistry::new();
    let resolver = RecordingResolver::new(b"bytes");
    registry.register_scheme("mem", resolver.clone()).unwrap();
    let bob = claims("tenant-b", "project-b", "bob");
    registry.resolve(&bob, "mem://k").await.unwrap();
    assert_eq!(
        resolver.seen.lock().unwrap().as_slice(),
        &[(
            "tenant-b".to_string(),
            "project-b".to_string(),
            "bob".to_string(),
            "mem://k".to_string()
        )]
    );
}

#[tokio::test]
async fn registry_caps_content_at_request_body_limit() {
    assert_eq!(MAX_INPUT_REF_BYTES, 64 * 1024);
    let registry = InputRefRegistry::new();
    registry
        .register_scheme(
            "ok",
            RecordingResolver::new(&vec![b'a'; MAX_INPUT_REF_BYTES]),
        )
        .unwrap();
    registry
        .register_scheme(
            "big",
            RecordingResolver::new(&vec![b'a'; MAX_INPUT_REF_BYTES + 1]),
        )
        .unwrap();
    let alice = claims("tenant-a", "project-a", "alice");
    assert_eq!(
        registry.resolve(&alice, "ok://k").await.unwrap().len(),
        MAX_INPUT_REF_BYTES
    );
    assert_eq!(
        registry.resolve(&alice, "big://k").await.unwrap_err(),
        InputRefError::TooLarge
    );
}

#[tokio::test]
async fn fetch_errors_use_fixed_messages_without_echo() {
    let registry = InputRefRegistry::new();
    for (scheme, error) in [
        ("nf", InputRefError::NotFound),
        ("tl", InputRefError::TooLarge),
        ("un", InputRefError::Unavailable),
    ] {
        registry
            .register_scheme(scheme, Arc::new(FailingResolver(error)))
            .unwrap();
    }
    registry
        .register_scheme("mem", RecordingResolver::new(b"payload"))
        .unwrap();
    let alice = claims("tenant-a", "project-a", "alice");
    let digest = sha256_hex(b"payload");

    for uri in [
        "nf://secret-tenant-b/key",
        "tl://k",
        "un://k",
        "none://k",
        "garbage",
    ] {
        let error = fetch_and_verify_input_ref(&registry, &alice, uri, &digest)
            .await
            .unwrap_err();
        assert_eq!(
            error,
            (
                INPUT_REF_FETCH_FAILED_ERROR_CODE,
                INPUT_REF_FETCH_FAILED_MESSAGE
            ),
            "{uri}"
        );
        assert!(!error.1.contains(uri));
    }

    let mismatch = fetch_and_verify_input_ref(&registry, &alice, "mem://k", &"0".repeat(64))
        .await
        .unwrap_err();
    assert_eq!(
        mismatch,
        (
            INPUT_DIGEST_MISMATCH_ERROR_CODE,
            INPUT_DIGEST_MISMATCH_MESSAGE
        )
    );
    assert!(!mismatch.1.contains(&digest));
    assert_eq!(
        fetch_and_verify_input_ref(&registry, &alice, "mem://k", &digest)
            .await
            .unwrap(),
        b"payload"
    );
}

#[test]
fn artifact_resolver_switch_defaults_off() {
    assert!(!artifact_resolver_enabled_from_vars(|_| None));
    for off in ["", "0", "false", "off", "no", "enabled"] {
        assert!(
            !artifact_resolver_enabled_from_vars(|_| Some(off.to_string())),
            "{off:?}"
        );
    }
    for on in ["1", "true", " TRUE ", "yes", "on"] {
        assert!(
            artifact_resolver_enabled_from_vars(|key| {
                (key == ARTIFACT_INPUT_REF_ENABLED_ENV).then(|| on.to_string())
            }),
            "{on:?}"
        );
    }
}

struct ArtifactFixture {
    kg_store: Arc<oxigraph::store::Store>,
    blob: Arc<LocalFsBlobStore>,
    _dir: tempfile::TempDir,
}

impl ArtifactFixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            kg_store: Arc::new(oxigraph::store::Store::new().unwrap()),
            blob: Arc::new(LocalFsBlobStore::new(dir.path().join("blobs"))),
            _dir: dir,
        }
    }

    fn resolver(&self) -> ArtifactInputRefResolver {
        ArtifactInputRefResolver::new(self.kg_store.clone(), self.blob.clone())
    }

    async fn put(&self, owner: &IsolationClaims, bytes: &[u8]) -> ArtifactMetadata {
        let id = uuid::Uuid::new_v4().hyphenated().to_string();
        let metadata = ArtifactMetadata {
            id: id.clone(),
            kind: ArtifactKind::RunTranscript,
            task_iri: "iri://task/input-ref".to_string(),
            blob_key: artifact_key_for_tests(&id, ArtifactKind::RunTranscript),
            content_type: "text/plain; charset=utf-8".to_string(),
            size_bytes: bytes.len(),
            sha256: sha256_hex(bytes),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by: owner.actor_id().to_string(),
        };
        self.blob
            .put(owner, &metadata.blob_key, bytes, &metadata.content_type)
            .await
            .unwrap();
        write_metadata_for_tests(&self.kg_store, owner, &metadata);
        metadata
    }
}

async fn resolve_with(
    resolver: &ArtifactInputRefResolver,
    who: &IsolationClaims,
    uri: &str,
) -> Result<Vec<u8>, InputRefError> {
    resolver
        .resolve(InputRefRequest {
            claims: who,
            uri,
            max_bytes: MAX_INPUT_REF_BYTES,
        })
        .await
}

#[tokio::test]
async fn artifact_resolver_reads_same_project_for_any_actor() {
    let fx = ArtifactFixture::new();
    let alice = claims("tenant-a", "project-a", "alice");
    let carol = claims("tenant-a", "project-a", "carol");
    let artifact = fx.put(&alice, b"transcript").await;
    let uri = format!("wao-artifact://{}", artifact.id);

    let resolver = fx.resolver();
    assert_eq!(
        resolve_with(&resolver, &alice, &uri).await.unwrap(),
        b"transcript"
    );
    assert_eq!(
        resolve_with(&resolver, &carol, &uri).await.unwrap(),
        b"transcript",
        "same tenant + project, different actor"
    );
}

#[tokio::test]
async fn artifact_resolver_hides_other_projects_and_tenants() {
    let fx = ArtifactFixture::new();
    let alice = claims("tenant-a", "project-a", "alice");
    let artifact = fx.put(&alice, b"secret").await;
    let uri = format!("wao-artifact://{}", artifact.id);
    let resolver = fx.resolver();

    let other_project = claims("tenant-a", "project-b", "alice");
    let other_tenant = claims("tenant-b", "project-a", "mallory");
    let unknown = format!("wao-artifact://{}", uuid::Uuid::new_v4().hyphenated());
    for (who, uri) in [
        (&other_project, uri.as_str()),
        (&other_tenant, uri.as_str()),
        (&alice, unknown.as_str()),
    ] {
        assert_eq!(
            resolve_with(&resolver, who, uri).await.unwrap_err(),
            InputRefError::NotFound,
            "{who:?} {uri}"
        );
    }
}

#[tokio::test]
async fn artifact_resolver_rejects_malformed_ids() {
    let fx = ArtifactFixture::new();
    let alice = claims("tenant-a", "project-a", "alice");
    let artifact = fx.put(&alice, b"x").await;
    let resolver = fx.resolver();
    let upper = artifact.id.to_uppercase();
    let simple = artifact.id.replace('-', "");
    for uri in [
        format!("wao-artifact://{upper}"),
        format!("wao-artifact://{simple}"),
        format!("wao-artifact://{}/extra", artifact.id),
        format!("wao-artifact://{}?x=1", artifact.id),
        format!("wao-artifact://../{}", artifact.id),
        "wao-artifact://not-a-uuid".to_string(),
        format!("other://{}", artifact.id),
    ] {
        assert_eq!(
            resolve_with(&resolver, &alice, &uri).await.unwrap_err(),
            InputRefError::NotFound,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn artifact_resolver_enforces_size_and_canonical_key() {
    let fx = ArtifactFixture::new();
    let alice = claims("tenant-a", "project-a", "alice");
    let big = fx.put(&alice, &vec![b'a'; MAX_INPUT_REF_BYTES + 1]).await;
    let resolver = fx.resolver();
    assert_eq!(
        resolve_with(&resolver, &alice, &format!("wao-artifact://{}", big.id))
            .await
            .unwrap_err(),
        InputRefError::TooLarge
    );

    // Metadata pointing at a key the upload route would not mint is refused,
    // even when that blob exists under the caller's tenant.
    let id = uuid::Uuid::new_v4().hyphenated().to_string();
    fx.blob
        .put(&alice, "kb/other/blob", b"not-an-artifact", "text/plain")
        .await
        .unwrap();
    let forged = ArtifactMetadata {
        id: id.clone(),
        kind: ArtifactKind::Patch,
        task_iri: "iri://task/x".to_string(),
        blob_key: "kb/other/blob".to_string(),
        content_type: "text/plain".to_string(),
        size_bytes: 15,
        sha256: sha256_hex(b"not-an-artifact"),
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by: "alice".to_string(),
    };
    write_metadata_for_tests(&fx.kg_store, &alice, &forged);
    assert_eq!(
        resolve_with(&resolver, &alice, &format!("wao-artifact://{id}"))
            .await
            .unwrap_err(),
        InputRefError::NotFound
    );
}

#[tokio::test]
async fn startup_registry_is_empty_unless_switched_on_with_a_blob_store() {
    let fx = ArtifactFixture::new();
    let off = startup_input_ref_registry(fx.kg_store.clone(), Some(fx.blob.clone()), false);
    assert!(!off.has_resolver_for("wao-artifact://x"));
    let no_blob = startup_input_ref_registry(fx.kg_store.clone(), None, true);
    assert!(!no_blob.has_resolver_for("wao-artifact://x"));

    let on = startup_input_ref_registry(fx.kg_store.clone(), Some(fx.blob.clone()), true);
    assert!(on.has_resolver_for("wao-artifact://x"));
    assert!(!on.has_resolver_for("s3://bucket/key"), "only the built-in");

    let alice = claims("tenant-a", "project-a", "alice");
    let artifact = fx.put(&alice, b"end-to-end").await;
    let bytes = fetch_and_verify_input_ref(
        &on,
        &alice,
        &format!("wao-artifact://{}", artifact.id),
        &artifact.sha256,
    )
    .await
    .unwrap();
    assert_eq!(bytes, b"end-to-end");
}

#[tokio::test]
async fn startup_registry_picks_up_embedder_registrations() {
    // Unique scheme: the deployment registry is process-wide.
    deployment_input_ref_registry()
        .register_scheme(
            "test-embedder-7c1f",
            RecordingResolver::new(b"from-embedder"),
        )
        .unwrap();
    let fx = ArtifactFixture::new();
    let registry = startup_input_ref_registry(fx.kg_store.clone(), None, false);
    assert!(registry.has_resolver_for("test-embedder-7c1f://k"));
    let alice = claims("tenant-a", "project-a", "alice");
    assert_eq!(
        registry
            .resolve(&alice, "test-embedder-7c1f://k")
            .await
            .unwrap(),
        b"from-embedder"
    );
}

fn invocation(request: serde_json::Value) -> Invocation {
    serde_json::from_value(json!({
        "id": "inv_test",
        "object": "invocation",
        "tenant_id": "tenant-a",
        "project_id": "project-a",
        "actor_id": "alice",
        "state": "queued",
        "revision": 1,
        "request": request,
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z"
    }))
    .unwrap()
}

#[test]
fn resolved_content_stands_in_for_inline_input() {
    let with_ref = invocation(json!({
        "input_ref": {"uri": "mem://k", "sha256": "a".repeat(64)}
    }));
    assert_eq!(
        prompt_from_invocation(&with_ref, Some(b"from the ref")),
        "from the ref"
    );
    assert_eq!(
        prompt_from_invocation(&with_ref, Some(&[0x66, 0xff, 0x6f])),
        "f\u{fffd}o"
    );
    let with_prompt = invocation(json!({"prompt": "explicit"}));
    assert_eq!(
        prompt_from_invocation(&with_prompt, Some(b"ignored")),
        "explicit"
    );
    assert_eq!(prompt_from_invocation(&with_prompt, None), "explicit");
}
