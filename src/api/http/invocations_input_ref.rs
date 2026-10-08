//! `input_ref` resolver extension point and the built-in artifact resolver.
//!
//! A create may reference its input instead of inlining it:
//! `input_ref = { uri, sha256 }`. The execution bridge asks this registry to
//! fetch the bytes, then checks the pinned digest.
//!
//! # Extension point
//!
//! - [`InputRefResolver`] is the trait every resolver implements, built-in or
//!   external. It is always called with the **caller's verified claims**
//!   ([`InputRefRequest::claims`]) next to the URI, so a resolver can (and
//!   must) scope its lookup to the caller; it never sees a bare address.
//! - [`InputRefRegistry`] routes a URI to one resolver. Resolvers register
//!   either by exact scheme ([`InputRefRegistry::register_scheme`]) or by URI
//!   prefix ([`InputRefRegistry::register_prefix`]). The longest matching
//!   prefix wins; otherwise the exact scheme; otherwise nothing matches.
//!   Registration is first-come: a second registration of the same scheme or
//!   prefix is rejected, so nothing can silently replace the built-in.
//! - Fail closed: when no resolver matches, create returns
//!   `422 input_ref_unresolvable` and nothing is persisted. Every runtime
//!   failure (not found, not yours, too large, backend error, no match) ends
//!   the invocation `failed` / `input_ref_fetch_failed` with one fixed message,
//!   so the response never reveals whether someone else's data exists.
//! - Size: the registry rejects content above [`MAX_INPUT_REF_BYTES`] (the
//!   create request-body limit) whatever the resolver returns.
//! - Embedders register external resolvers on
//!   [`deployment_input_ref_registry`] before `build_router`; startup copies
//!   them into the runtime registry after the built-in one.
//!
//! # Built-in resolver
//!
//! [`ArtifactInputRefResolver`] serves `wao-artifact://<artifact-id>` from the
//! platform's own claims-scoped artifact store (`/api/v1/artifacts`). It is
//! **off by default** (`AGENTOS_INVOCATION_INPUT_REF_ARTIFACTS_ENABLED`) and
//! makes no outbound network request. An artifact resolves only for the same
//! tenant and project (any actor in that project); another project or tenant
//! gets the same failure as an unknown id.

use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use dashmap::DashMap;
use sha2::{Digest, Sha256};

use crate::blob::BlobStore;
use crate::isolation::IsolationClaims;

/// Resolver failed, matched nothing, or returned too much (before the digest
/// check). Same code for every cause.
pub const INPUT_REF_FETCH_FAILED_ERROR_CODE: &str =
    super::invocations_store::INPUT_REF_FETCH_FAILED_ERROR_CODE;
/// Fetched bytes do not match the pinned sha256.
pub const INPUT_DIGEST_MISMATCH_ERROR_CODE: &str =
    super::invocations_store::INPUT_DIGEST_MISMATCH_ERROR_CODE;
/// Fixed, safe message for every `input_ref_fetch_failed`.
pub const INPUT_REF_FETCH_FAILED_MESSAGE: &str = "input_ref could not be resolved";
/// Fixed, safe message for every `input_digest_mismatch`.
pub const INPUT_DIGEST_MISMATCH_MESSAGE: &str = "input_ref content does not match sha256";

/// Largest `input_ref` content accepted, equal to the create request-body
/// limit (`MAX_CREATE_BODY_BYTES`, 64 KiB).
pub const MAX_INPUT_REF_BYTES: usize = super::invocations::MAX_CREATE_BODY_BYTES;

/// Scheme served by [`ArtifactInputRefResolver`].
pub const ARTIFACT_INPUT_REF_SCHEME: &str = "wao-artifact";
/// Env switch for the built-in artifact resolver (default off).
pub const ARTIFACT_INPUT_REF_ENABLED_ENV: &str = "AGENTOS_INVOCATION_INPUT_REF_ARTIFACTS_ENABLED";

/// One resolution request. `claims` are the verified claims of the caller
/// that created the invocation; resolvers must scope every lookup to them.
#[derive(Debug, Clone, Copy)]
pub struct InputRefRequest<'a> {
    pub claims: &'a IsolationClaims,
    pub uri: &'a str,
    /// Upper bound on the content; a resolver may stop early above it.
    pub max_bytes: usize,
}

/// Why a resolution failed. Internal only: callers of the HTTP API always see
/// [`INPUT_REF_FETCH_FAILED_MESSAGE`], never this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRefError {
    /// Unknown, malformed, or not visible to these claims (indistinguishable
    /// on purpose).
    NotFound,
    /// Content is larger than `max_bytes`.
    TooLarge,
    /// Backend not configured or failed.
    Unavailable,
    /// No registered resolver matches the URI.
    NoResolver,
}

/// Fetches the bytes behind an `input_ref` URI for one caller.
#[async_trait]
pub trait InputRefResolver: Send + Sync {
    async fn resolve(&self, request: InputRefRequest<'_>) -> Result<Vec<u8>, InputRefError>;
}

/// Registration rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRefRegistrationError {
    /// Scheme is not lowercase `[a-z][a-z0-9+.-]*`.
    InvalidScheme,
    /// Prefix is not `<valid scheme>://<at least one more character>`.
    InvalidPrefix,
    /// The scheme or prefix is already registered.
    Duplicate,
}

impl std::fmt::Display for InputRefRegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidScheme => "input_ref scheme must match [a-z][a-z0-9+.-]*",
            Self::InvalidPrefix => "input_ref prefix must be <scheme>://<non-empty>",
            Self::Duplicate => "input_ref scheme or prefix is already registered",
        })
    }
}

impl std::error::Error for InputRefRegistrationError {}

type SharedResolver = Arc<dyn InputRefResolver>;

/// Scheme / prefix → resolver routing. Cheap to clone (shared tables).
#[derive(Clone, Default)]
pub struct InputRefRegistry {
    by_scheme: Arc<DashMap<String, SharedResolver>>,
    /// Kept sorted by descending prefix length so the first match is the
    /// longest.
    by_prefix: Arc<RwLock<Vec<(String, SharedResolver)>>>,
}

impl std::fmt::Debug for InputRefRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputRefRegistry")
            .field("schemes", &self.schemes())
            .field("prefixes", &self.prefixes())
            .finish()
    }
}

fn valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))
}

impl InputRefRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Routes every URI `<scheme>://…` to `resolver` unless a longer prefix
    /// registration matches first.
    pub fn register_scheme(
        &self,
        scheme: impl Into<String>,
        resolver: Arc<dyn InputRefResolver>,
    ) -> Result<(), InputRefRegistrationError> {
        let scheme = scheme.into();
        if !valid_scheme(&scheme) {
            return Err(InputRefRegistrationError::InvalidScheme);
        }
        match self.by_scheme.entry(scheme) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(InputRefRegistrationError::Duplicate),
            dashmap::mapref::entry::Entry::Vacant(slot) => {
                slot.insert(resolver);
                Ok(())
            }
        }
    }

    /// Routes every URI that starts with `prefix` (for example
    /// `s3://bucket-a/`) to `resolver`. Plain byte-prefix match.
    pub fn register_prefix(
        &self,
        prefix: impl Into<String>,
        resolver: Arc<dyn InputRefResolver>,
    ) -> Result<(), InputRefRegistrationError> {
        let prefix = prefix.into();
        let valid = Self::scheme_of(&prefix).is_some_and(valid_scheme);
        if !valid {
            return Err(InputRefRegistrationError::InvalidPrefix);
        }
        let mut table = self
            .by_prefix
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if table.iter().any(|(existing, _)| *existing == prefix) {
            return Err(InputRefRegistrationError::Duplicate);
        }
        table.push((prefix, resolver));
        table.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(())
    }

    /// Copies every registration of `other` into `self`. Entries that clash
    /// with an existing registration are skipped and returned.
    pub fn extend_from(&self, other: &InputRefRegistry) -> Vec<String> {
        let mut skipped = Vec::new();
        let schemes: Vec<(String, SharedResolver)> = other
            .by_scheme
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();
        for (scheme, resolver) in schemes {
            if self.register_scheme(scheme.clone(), resolver).is_err() {
                skipped.push(scheme);
            }
        }
        let prefixes: Vec<(String, SharedResolver)> = other
            .by_prefix
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for (prefix, resolver) in prefixes {
            if self.register_prefix(prefix.clone(), resolver).is_err() {
                skipped.push(prefix);
            }
        }
        skipped
    }

    /// Registered schemes, sorted.
    pub fn schemes(&self) -> Vec<String> {
        let mut out: Vec<String> = self.by_scheme.iter().map(|e| e.key().clone()).collect();
        out.sort();
        out
    }

    /// Registered prefixes, longest first.
    pub fn prefixes(&self) -> Vec<String> {
        self.by_prefix
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(prefix, _)| prefix.clone())
            .collect()
    }

    /// Extracts the scheme from `uri` (`scheme://…`). `None` if malformed.
    pub fn scheme_of(uri: &str) -> Option<&str> {
        let (scheme, rest) = uri.split_once("://")?;
        if scheme.is_empty() || rest.is_empty() {
            return None;
        }
        Some(scheme)
    }

    /// The resolver `uri` routes to: longest prefix, then exact scheme.
    fn matching(&self, uri: &str) -> Option<SharedResolver> {
        let by_prefix = self
            .by_prefix
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(prefix, _)| uri.starts_with(prefix.as_str()))
            .map(|(_, resolver)| resolver.clone());
        by_prefix.or_else(|| {
            let scheme = Self::scheme_of(uri)?;
            self.by_scheme
                .get(scheme)
                .map(|entry| entry.value().clone())
        })
    }

    /// Whether some registered resolver would handle `uri` (create-time check).
    pub fn has_resolver_for(&self, uri: &str) -> bool {
        self.matching(uri).is_some()
    }

    /// Resolves `uri` for `claims` and enforces [`MAX_INPUT_REF_BYTES`].
    pub async fn resolve(
        &self,
        claims: &IsolationClaims,
        uri: &str,
    ) -> Result<Vec<u8>, InputRefError> {
        let resolver = self.matching(uri).ok_or(InputRefError::NoResolver)?;
        let bytes = resolver
            .resolve(InputRefRequest {
                claims,
                uri,
                max_bytes: MAX_INPUT_REF_BYTES,
            })
            .await?;
        if bytes.len() > MAX_INPUT_REF_BYTES {
            return Err(InputRefError::TooLarge);
        }
        Ok(bytes)
    }
}

/// Process-wide registry for embedders. Register external resolvers here
/// before calling `build_router`; startup copies them into the runtime
/// registry (after the built-in resolver, which therefore cannot be
/// replaced). Registrations made after startup are not picked up.
pub fn deployment_input_ref_registry() -> &'static InputRefRegistry {
    static REGISTRY: OnceLock<InputRefRegistry> = OnceLock::new();
    REGISTRY.get_or_init(InputRefRegistry::new)
}

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Fetches `input_ref` for `claims` and checks the pinned digest.
/// Errors carry a typed code and a fixed message; nothing from the URI,
/// the resolver or the content is echoed.
pub(crate) async fn fetch_and_verify_input_ref(
    registry: &InputRefRegistry,
    claims: &IsolationClaims,
    uri: &str,
    expected_sha256: &str,
) -> Result<Vec<u8>, (&'static str, &'static str)> {
    let bytes = registry.resolve(claims, uri).await.map_err(|error| {
        tracing::debug!(?error, "input_ref resolution failed");
        (
            INPUT_REF_FETCH_FAILED_ERROR_CODE,
            INPUT_REF_FETCH_FAILED_MESSAGE,
        )
    })?;
    if sha256_hex(&bytes) != expected_sha256 {
        return Err((
            INPUT_DIGEST_MISMATCH_ERROR_CODE,
            INPUT_DIGEST_MISMATCH_MESSAGE,
        ));
    }
    Ok(bytes)
}

/// Reads [`ARTIFACT_INPUT_REF_ENABLED_ENV`]; truthy: `1`, `true`, `yes`,
/// `on` (case-insensitive). Default off.
pub(crate) fn artifact_resolver_enabled_from_vars(lookup: impl Fn(&str) -> Option<String>) -> bool {
    lookup(ARTIFACT_INPUT_REF_ENABLED_ENV)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Built-in resolver for `wao-artifact://<artifact-id>`.
///
/// Looks the id up in the caller's claims graph (tenant + project), then reads
/// the bytes under the caller's tenant blob prefix. Anything else — malformed
/// id, another project's or tenant's artifact, missing blob — is
/// [`InputRefError::NotFound`]. Reads only local platform storage.
pub struct ArtifactInputRefResolver {
    kg_store: Arc<oxigraph::store::Store>,
    blob_store: Arc<dyn BlobStore>,
}

impl ArtifactInputRefResolver {
    pub fn new(kg_store: Arc<oxigraph::store::Store>, blob_store: Arc<dyn BlobStore>) -> Self {
        Self {
            kg_store,
            blob_store,
        }
    }

    /// Accepts exactly `wao-artifact://<lowercase hyphenated uuid>`.
    fn artifact_id(uri: &str) -> Option<&str> {
        let id = uri.strip_prefix("wao-artifact://")?;
        let parsed = uuid::Uuid::parse_str(id).ok()?;
        (parsed.hyphenated().to_string() == id).then_some(id)
    }
}

#[async_trait]
impl InputRefResolver for ArtifactInputRefResolver {
    async fn resolve(&self, request: InputRefRequest<'_>) -> Result<Vec<u8>, InputRefError> {
        let id = Self::artifact_id(request.uri).ok_or(InputRefError::NotFound)?;
        let metadata = super::artifacts::load_artifact_metadata(&self.kg_store, request.claims, id)
            .map_err(|_| InputRefError::Unavailable)?
            .ok_or(InputRefError::NotFound)?;
        if metadata.size_bytes > request.max_bytes {
            return Err(InputRefError::TooLarge);
        }
        if !super::artifacts::is_canonical_blob_key(&metadata) {
            return Err(InputRefError::NotFound);
        }
        let bytes = self
            .blob_store
            .get(request.claims, &metadata.blob_key)
            .await
            .map_err(|_| InputRefError::NotFound)?;
        if bytes.len() > request.max_bytes {
            return Err(InputRefError::TooLarge);
        }
        Ok(bytes)
    }
}

/// Builds the runtime registry at startup: the built-in artifact resolver when
/// enabled and a blob store is configured, then every embedder registration
/// from [`deployment_input_ref_registry`].
pub(crate) fn startup_input_ref_registry(
    kg_store: Arc<oxigraph::store::Store>,
    blob_store: Option<Arc<dyn BlobStore>>,
    artifacts_enabled: bool,
) -> InputRefRegistry {
    let registry = InputRefRegistry::new();
    if artifacts_enabled {
        match blob_store {
            Some(blob_store) => {
                let resolver = Arc::new(ArtifactInputRefResolver::new(kg_store, blob_store));
                if registry
                    .register_scheme(ARTIFACT_INPUT_REF_SCHEME, resolver)
                    .is_ok()
                {
                    tracing::warn!(
                        "built-in input_ref resolver wao-artifact:// enabled via {ARTIFACT_INPUT_REF_ENABLED_ENV}"
                    );
                }
            }
            None => tracing::warn!(
                "{ARTIFACT_INPUT_REF_ENABLED_ENV} is set but no blob store is configured; wao-artifact:// stays unresolvable"
            ),
        }
    }
    for skipped in registry.extend_from(deployment_input_ref_registry()) {
        tracing::error!(
            registration = %skipped,
            "embedder input_ref registration clashes with an existing one and was skipped"
        );
    }
    registry
}

#[cfg(test)]
#[path = "invocations_input_ref_tests.rs"]
mod tests;
