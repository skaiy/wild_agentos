//! `input_snapshot` kind, optional `task_iri`, and list kind filter tests.

use super::tests::test_state;
use super::*;
use crate::api::http::iam::test_identity_from_verified_claims;

struct Fixture {
    root: std::path::PathBuf,
    state: Arc<AppState>,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("artifact-snap-{}", uuid::Uuid::new_v4()));
        let state = test_state(root.clone());
        Self { root, state }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn claims(tenant: &str) -> IsolationClaims {
    IsolationClaims::from_verified(tenant, "project", "actor").unwrap()
}

fn identity(claims: &IsolationClaims) -> UserIdentity {
    test_identity_from_verified_claims(claims.clone(), vec![])
}

async fn body_of(response: Response) -> (StatusCode, Value, axum::http::HeaderMap, Vec<u8>) {
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json, headers, bytes)
}

async fn upload(
    fx: &Fixture,
    claims: &IsolationClaims,
    kind: ArtifactKind,
    task_iri: Option<&str>,
    content: &[u8],
) -> (StatusCode, Value) {
    let response = upload_artifact_handler(
        State(fx.state.clone()),
        identity(claims),
        Json(ArtifactUploadRequest {
            kind,
            task_iri: task_iri.map(str::to_string),
            content_base64: STANDARD.encode(content),
        }),
    )
    .await;
    let (status, json, _, _) = body_of(response).await;
    (status, json)
}

async fn list(fx: &Fixture, claims: &IsolationClaims, kind: Option<&str>) -> (StatusCode, Value) {
    let response = list_artifacts_handler(
        State(fx.state.clone()),
        identity(claims),
        Query(ListArtifactsQuery {
            kind: kind.map(str::to_string),
        }),
    )
    .await;
    let (status, json, _, _) = body_of(response).await;
    (status, json)
}

const SNAPSHOT: &[u8] = br#"{"schema":"snapshot/v1","items":[{"id":"t-1","level":"low"}]}"#;

#[test]
fn input_snapshot_kind_is_json_and_task_iri_is_optional_only_for_it() {
    assert_eq!(
        ArtifactKind::InputSnapshot.content_type(),
        "application/json"
    );
    assert_eq!(ArtifactKind::InputSnapshot.extension(), "json");
    assert_eq!(
        ArtifactKind::parse("input_snapshot"),
        Some(ArtifactKind::InputSnapshot)
    );
    assert_eq!(ArtifactKind::parse("InputSnapshot"), None);
    assert!(!ArtifactKind::InputSnapshot.requires_task_iri());
    for kind in [
        ArtifactKind::Patch,
        ArtifactKind::RunTranscript,
        ArtifactKind::ReproduceScript,
    ] {
        assert!(kind.requires_task_iri());
    }
}

#[test]
fn legacy_metadata_records_still_deserialize() {
    let legacy = json!({
        "id": "00000000-0000-4000-8000-000000000000", "kind": "patch",
        "task_iri": "iri://task/1", "blob_key": "artifacts/x.patch",
        "content_type": "text/x-diff; charset=utf-8", "size_bytes": 1,
        "sha256": "00", "created_at": "2026-01-01T00:00:00Z", "created_by": "a"
    });
    let parsed: ArtifactMetadata = serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(parsed.task_iri.as_deref(), Some("iri://task/1"));
    let mut without = legacy;
    without.as_object_mut().unwrap().remove("task_iri");
    let parsed: ArtifactMetadata = serde_json::from_value(without).unwrap();
    assert_eq!(parsed.task_iri, None);
}

#[tokio::test]
async fn snapshot_without_task_iri_round_trips_as_json() {
    let fx = Fixture::new();
    let tenant = claims("tenant-a");
    let (status, body) = upload(&fx, &tenant, ArtifactKind::InputSnapshot, None, SNAPSHOT).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let artifact = &body["artifact"];
    assert_eq!(artifact["kind"], "input_snapshot");
    assert_eq!(artifact["content_type"], "application/json");
    assert_eq!(artifact["task_iri"], Value::Null);
    // `input_ref.sha256` can use this digest directly: it covers the raw bytes.
    assert_eq!(artifact["sha256"], hex::encode(Sha256::digest(SNAPSHOT)));
    let id = artifact["id"].as_str().unwrap().to_string();
    assert!(artifact["blob_key"].as_str().unwrap().ends_with(".json"));

    let response =
        download_artifact_handler(State(fx.state.clone()), identity(&tenant), Path(id.clone()))
            .await;
    let (status, _, headers, bytes) = body_of(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, SNAPSHOT);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(
        headers[header::CONTENT_DISPOSITION],
        format!("attachment; filename=\"{id}.json\"").as_str()
    );
}

#[tokio::test]
async fn snapshot_accepts_caller_business_iri_and_still_shape_checks_it() {
    let fx = Fixture::new();
    let tenant = claims("tenant-a");
    let urn = "urn:example:snapshot:42@3";
    let (status, body) = upload(
        &fx,
        &tenant,
        ArtifactKind::InputSnapshot,
        Some(urn),
        SNAPSHOT,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["artifact"]["task_iri"], urn);
    for bad in ["", "  ", "urn:x\n:y"] {
        let (status, _) = upload(
            &fx,
            &tenant,
            ArtifactKind::InputSnapshot,
            Some(bad),
            SNAPSHOT,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}");
    }
}

#[tokio::test]
async fn invalid_snapshot_content_and_missing_replay_task_iri_are_rejected_unpersisted() {
    let fx = Fixture::new();
    let tenant = claims("tenant-a");
    for content in [
        &b"not json"[..],
        b"{\"a\":1",
        b"\"\xff\xfe\"",
        b"\xef\xbb\xbf{}",
    ] {
        let (status, body) = upload(&fx, &tenant, ArtifactKind::InputSnapshot, None, content).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{content:?}");
        assert_eq!(
            body["error"],
            "input_snapshot content must be valid UTF-8 JSON"
        );
    }
    for kind in [
        ArtifactKind::Patch,
        ArtifactKind::RunTranscript,
        ArtifactKind::ReproduceScript,
    ] {
        let (status, body) = upload(&fx, &tenant, kind, None, b"diff --git\n").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "task_iri is required for this artifact kind");
    }
    let (_, body) = list(&fx, &tenant, None).await;
    assert_eq!(body["count"], 0);
}

#[tokio::test]
async fn list_filters_by_kind_within_claims_only() {
    let fx = Fixture::new();
    let tenant_a = claims("tenant-a");
    let tenant_b = claims("tenant-b");
    upload(&fx, &tenant_a, ArtifactKind::InputSnapshot, None, SNAPSHOT).await;
    upload(
        &fx,
        &tenant_a,
        ArtifactKind::Patch,
        Some("iri://task/1"),
        b"diff --git\n",
    )
    .await;
    upload(&fx, &tenant_b, ArtifactKind::InputSnapshot, None, SNAPSHOT).await;

    let (status, all) = list(&fx, &tenant_a, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["count"], 2);

    let (status, snaps) = list(&fx, &tenant_a, Some("input_snapshot")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snaps["count"], 1);
    assert_eq!(snaps["artifacts"][0]["kind"], "input_snapshot");

    let (_, patches) = list(&fx, &tenant_a, Some("patch")).await;
    assert_eq!(patches["count"], 1);
    let (_, scripts) = list(&fx, &tenant_a, Some("reproduce_script")).await;
    assert_eq!(scripts["count"], 0);

    for unknown in ["", "snapshot", "INPUT_SNAPSHOT"] {
        let (status, body) = list(&fx, &tenant_a, Some(unknown)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{unknown:?}");
        assert_eq!(body["error"], "unknown artifact kind");
    }
}

fn nested_array(depth: usize) -> Vec<u8> {
    format!("{}{}", "[".repeat(depth), "]".repeat(depth)).into_bytes()
}

#[test]
fn snapshot_depth_limit_matches_value_parsing() {
    // Accepted iff a later `serde_json::Value` parse succeeds.
    for depth in [1, 64, INPUT_SNAPSHOT_MAX_DEPTH] {
        let bytes = nested_array(depth);
        assert!(is_valid_snapshot_json(&bytes), "depth {depth}");
        assert!(serde_json::from_slice::<Value>(&bytes).is_ok());
    }
    for depth in [INPUT_SNAPSHOT_MAX_DEPTH + 1, 10_000] {
        let bytes = nested_array(depth);
        assert!(!is_valid_snapshot_json(&bytes), "depth {depth}");
        assert!(serde_json::from_slice::<Value>(&bytes).is_err());
    }
    let deep_object = format!(
        "{}1{}",
        "{\"a\":".repeat(INPUT_SNAPSHOT_MAX_DEPTH + 1),
        "}".repeat(INPUT_SNAPSHOT_MAX_DEPTH + 1)
    );
    assert!(!is_valid_snapshot_json(deep_object.as_bytes()));
    // Brackets inside strings (including after escaped quotes) do not count.
    let in_strings = format!("[\"{}\", \"\\\"{}\"]", "[".repeat(500), "{".repeat(500));
    assert!(!json_nesting_exceeds(in_strings.as_bytes(), 1));
    assert!(is_valid_snapshot_json(in_strings.as_bytes()));
}

#[tokio::test]
async fn overly_deep_snapshot_is_rejected_unpersisted() {
    let fx = Fixture::new();
    let tenant = claims("tenant-a");
    let (status, body) = upload(
        &fx,
        &tenant,
        ArtifactKind::InputSnapshot,
        None,
        &nested_array(INPUT_SNAPSHOT_MAX_DEPTH + 1),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "input_snapshot content must be valid UTF-8 JSON"
    );
    let (status, body) = upload(
        &fx,
        &tenant,
        ArtifactKind::InputSnapshot,
        None,
        &nested_array(INPUT_SNAPSHOT_MAX_DEPTH),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, listed) = list(&fx, &tenant, None).await;
    assert_eq!(listed["count"], 1);
}
