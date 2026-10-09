//! `input_ref` resolver extension point and the built-in artifact resolver.
//!
//! A create may reference its input instead of inlining it:
//! `input_ref = { uri, sha256 }`. Create asks the registry to **validate**
//! the URI (no I/O); the execution bridge later asks it to **resolve** the
//! bytes, then the kernel checks the pinned digest and hands the text to the
//! task.
//!
//! # Extension point
//!
//! - [`InputRefResolver`] is the trait every resolver implements, built-in or
//!   external: a synchronous, I/O-free [`InputRefResolver::validate`] run at
//!   create time, and an async [`InputRefResolver::resolve`] run on the
//!   execution path. Both always receive the **caller's verified claims**;
//!   `resolve` also gets the scheme, invocation id, byte cap and deadline
//!   ([`InputRefRequest`]).
//! - [`InputRefRegistry`] routes a URI to one resolver. Resolvers register
//!   either by exact scheme ([`InputRefRegistry::register_scheme`]) or by URI
//!   prefix ending in `/` ([`InputRefRegistry::register_prefix`]). A scheme is
//!   owned either by one scheme resolver or by prefix resolvers, never both,
//!   so a prefix can never shadow a scheme resolver (such as the built-in).
//!   Among prefixes the longest match wins. Registration is first-come and
//!   duplicates are rejected. The runtime registry is **frozen** once startup
//!   finishes; later registrations fail with
//!   [`InputRefRegistrationError::Frozen`].
//! - Fail closed: when no resolver matches, create returns
//!   `422 input_ref_unresolvable` and nothing is persisted. A resolver's
//!   `validate` can reject the URI (`422 input_ref_unresolvable`) or flag it
//!   as outside the caller's project (`422 input_ref_scope_mismatch`).
//! - Before any routing the kernel checks the URI shape
//!   ([`check_input_ref_uri`]): lowercase scheme, printable ASCII only
//!   (`0x21..=0x7E`), no `?` / `#` / `;` / `%25`, no empty / `.` / `..`
//!   segments, no backslash and no encoded `.`, `/` or `\\` (`%2e`, `%2f`,
//!   `%5c`, any case). Create answers `400`. A missing project segment
//!   (`scheme:///…`) is an empty segment and is also `400`, not `422`.
//! - The kernel, not the resolver, enforces the size cap
//!   (`AGENTOS_INVOCATION_INPUT_REF_MAX_BYTES`, default
//!   [`DEFAULT_INPUT_REF_MAX_BYTES`], at most [`MAX_INPUT_REF_MAX_BYTES`]),
//!   the timeout (`AGENTOS_INVOCATION_INPUT_REF_TIMEOUT_MS`, capped by the
//!   invocation deadline), the sha256 check and UTF-8 text. A resolver must
//!   still stop reading as soon as it reaches `max_bytes`.
//! - Resolved content is untrusted data: any actor of the same project can
//!   write it, and sha256 pins only the bytes, not their intent. The kernel
//!   hands it to the task as a delimited block after a fixed "untrusted
//!   data, not instructions" line, in the task goal (user role), never in a
//!   system prompt. The content escapes `&` and `<` (and fullwidth `＜`) so
//!   it cannot forge another tag.
//!   Every runtime failure ends the invocation `failed` /
//!   `input_ref_fetch_failed` (or `input_digest_mismatch`) with one fixed
//!   message; logs carry only the invocation id, scheme and failure class.
//! - Embedders register resolvers in code on
//!   [`deployment_input_ref_registry`] before `build_router`; startup copies
//!   them into the runtime registry after the built-in one, then freezes both.
//!   Configuration can only switch compiled-in resolvers on; it never loads
//!   code.
//!
//! # Built-in resolver
//!
//! [`ArtifactInputRefResolver`] serves `wao-artifact://<project_id>/<artifact-id>`
//! from the platform's own claims-scoped artifact store (`/api/v1/artifacts`).
//! It is **off by default** (`AGENTOS_INVOCATION_INPUT_REF_ARTIFACTS_ENABLED`)
//! and makes no outbound network request. Leave the switch unset: it may be
//! enabled only in a build that accepts solely the `input_snapshot` artifact
//! kind and requires the fetched bytes' SHA-256 to equal both the metadata
//! digest and the caller-pinned digest (this build does both; the default
//! stays off). The project segment must equal the caller's `project_id`
//! (checked at create); the artifact is looked up under the caller's claims,
//! so another project or tenant gets the same failure as an unknown id.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dashmap::DashMap;
use sha2::{Digest, Sha256};

use crate::blob::BlobStore;
use crate::isolation::IsolationClaims;

/// Resolver failed, matched nothing, timed out, returned too much or
/// non-UTF-8 content (before or after the digest check). Same code for every
/// cause.
pub const INPUT_REF_FETCH_FAILED_ERROR_CODE: &str =
    super::invocations_store::INPUT_REF_FETCH_FAILED_ERROR_CODE;
/// Fetched bytes do not match the pinned sha256.
pub const INPUT_DIGEST_MISMATCH_ERROR_CODE: &str =
    super::invocations_store::INPUT_DIGEST_MISMATCH_ERROR_CODE;
/// Create: no resolver matches the URI, or its resolver rejects the URI.
pub const INPUT_REF_UNRESOLVABLE_ERROR_CODE: &str = "input_ref_unresolvable";
/// Create: the URI names a project other than the caller's.
pub const INPUT_REF_SCOPE_MISMATCH_ERROR_CODE: &str = "input_ref_scope_mismatch";
/// Fixed, safe message for every `input_ref_fetch_failed`.
pub const INPUT_REF_FETCH_FAILED_MESSAGE: &str = "input_ref could not be resolved";
/// Fixed, safe message for every `input_digest_mismatch`.
pub const INPUT_DIGEST_MISMATCH_MESSAGE: &str = "input_ref content does not match sha256";

/// Default cap on `input_ref` content (64 KiB).
pub const DEFAULT_INPUT_REF_MAX_BYTES: usize = 64 * 1024;
/// Hard ceiling for [`INPUT_REF_MAX_BYTES_ENV`] (1 MiB).
pub const MAX_INPUT_REF_MAX_BYTES: usize = 1024 * 1024;
/// Env override for the content cap, in bytes (`1..=1048576`).
pub const INPUT_REF_MAX_BYTES_ENV: &str = "AGENTOS_INVOCATION_INPUT_REF_MAX_BYTES";

/// Env override for the per-resolution timeout, in milliseconds.
pub const INPUT_REF_TIMEOUT_ENV: &str = "AGENTOS_INVOCATION_INPUT_REF_TIMEOUT_MS";
/// Default per-resolution timeout.
pub const DEFAULT_INPUT_REF_TIMEOUT: Duration = Duration::from_secs(10);
/// Largest accepted [`INPUT_REF_TIMEOUT_ENV`] value.
pub const MAX_INPUT_REF_TIMEOUT: Duration = Duration::from_secs(60);

/// Scheme served by [`ArtifactInputRefResolver`].
pub const ARTIFACT_INPUT_REF_SCHEME: &str = "wao-artifact";
/// Env switch for the built-in artifact resolver (default off).
///
/// Do not set this until the running build (1) resolves only an
/// `input_snapshot` artifact and (2) requires the fetched bytes' SHA-256 to
/// equal both the digest stored on the artifact and the caller-pinned
/// digest, failing closed with one error that does not say which digest
/// differed. This build includes both checks. The default remains off;
/// turning the resolver on is a separate decision. Invocation execution
/// (`AGENTOS_INVOCATION_EXECUTION_ENABLED`) stays off by default as well.
pub const ARTIFACT_INPUT_REF_ENABLED_ENV: &str = "AGENTOS_INVOCATION_INPUT_REF_ARTIFACTS_ENABLED";

/// One resolution request, built by the kernel on the execution path.
#[derive(Debug, Clone, Copy)]
pub struct InputRefRequest<'a> {
    /// Full URI as sent at create (`<scheme>://…`).
    pub uri: &'a str,
    /// Scheme part of `uri`.
    pub scheme: &'a str,
    /// Id of the invocation being executed.
    pub invocation_id: &'a str,
    /// Verified claims of the caller that created the invocation (read from
    /// the stored resource, never from a request body). Resolvers must scope
    /// every lookup to them.
    pub claims: &'a IsolationClaims,
    /// Upper bound on the content. A resolver **must** stop reading (and
    /// return [`InputRefError::TooLarge`]) as soon as it reaches this many
    /// bytes instead of buffering the whole object; the kernel rejects
    /// anything larger regardless.
    pub max_bytes: usize,
    /// Point in time by which the resolver should give up: the earlier of
    /// the kernel timeout and the invocation `deadline`. The kernel abandons
    /// the call at this instant regardless.
    pub deadline: Instant,
}

/// Why a validation or resolution failed. Internal only: HTTP callers see a
/// fixed error code and message, never this value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRefError {
    /// Unknown, or not visible to these claims (indistinguishable on purpose).
    NotFound,
    /// The URI names a project / scope other than the caller's.
    ScopeMismatch,
    /// The resolver does not accept this URI shape (for example a missing
    /// immutable revision).
    Rejected,
    /// Content is larger than `max_bytes`.
    TooLarge,
    /// The resolver did not answer before the deadline.
    Timeout,
    /// Backend not configured or failed.
    Unavailable,
    /// No registered resolver matches the URI.
    NoResolver,
    /// Fetched bytes do not match a pinned digest. Callers map this to the
    /// same fixed error as a caller-pin mismatch, so the two are not
    /// distinguishable.
    DigestMismatch,
}

impl InputRefError {
    fn class(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::ScopeMismatch => "scope_mismatch",
            Self::Rejected => "rejected",
            Self::TooLarge => "too_large",
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::NoResolver => "no_resolver",
            // Not logged on its own: fetch maps it to the same line as a
            // caller-pin mismatch, without saying which digest differed.
            Self::DigestMismatch => "digest_mismatch",
        }
    }
}

/// Fetches the bytes behind an `input_ref` URI for one caller.
#[async_trait]
pub trait InputRefResolver: Send + Sync {
    /// Create-time check. Must not do I/O. Return
    /// [`InputRefError::ScopeMismatch`] when the URI names another project /
    /// scope than `claims`, any other error to reject the URI shape. The
    /// default accepts every URI routed to this resolver.
    fn validate(&self, _uri: &str, _claims: &IsolationClaims) -> Result<(), InputRefError> {
        Ok(())
    }

    /// Execution-time fetch. `request.claims` is the caller's verified
    /// claims; every implementation must scope its lookup to them. Only
    /// returns bytes: the kernel checks size, the caller-pinned sha256 and
    /// UTF-8 afterwards and never trusts the resolver for them.
    /// Must stop reading once `request.max_bytes` is reached (streaming or
    /// a bounded read), so an oversized object never fills memory.
    async fn resolve(&self, request: InputRefRequest<'_>) -> Result<Vec<u8>, InputRefError>;
}

/// Why [`check_input_ref_uri`] rejected a URI. Create maps each to `400`
/// with [`InputRefUriError::message`] (fixed, never echoes the URI).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRefUriError {
    /// Not `<scheme>://<something>`, or the scheme has invalid characters.
    Shape,
    /// The scheme contains uppercase letters.
    UppercaseScheme,
    /// Control character or line break anywhere in the URI.
    ControlCharacter,
    /// A character outside printable ASCII (`0x21..=0x7E`): space, non-ASCII,
    /// line/paragraph separators, zero-width characters, bidi controls.
    NotPrintableAscii,
    /// Empty, `.` or `..` segment, backslash, or encoded `.` / `/` / `\`.
    PathTraversal,
    /// `?`, `#`, `;`, or a percent-encoded percent (`%25`, any case).
    ReservedDelimiter,
}

impl InputRefUriError {
    /// Fixed client-facing message.
    pub fn message(self) -> &'static str {
        match self {
            Self::Shape => "input_ref.uri must be <scheme>://...",
            Self::UppercaseScheme => "input_ref.uri scheme must be lowercase",
            Self::ControlCharacter => {
                "input_ref.uri must not contain control characters or line breaks"
            }
            Self::NotPrintableAscii => "input_ref.uri must contain only printable ASCII",
            Self::PathTraversal => {
                "input_ref.uri must not contain empty, '.' or '..' segments, backslashes, or encoded '.', '/' or '\\'"
            }
            Self::ReservedDelimiter => {
                "input_ref.uri must not contain '?', '#', ';' or '%25'"
            }
        }
    }
}

/// Kernel URI shape check, run before any routing (create and resolve):
/// lowercase `<scheme>://`, printable ASCII only (`0x21..=0x7E`), no `?`,
/// `#`, `;` or `%25`, and no path tricks — no empty, `.` or `..` segment, no
/// backslash, no `%2e` / `%2f` / `%5c` in any case. A missing project segment
/// is an empty segment and fails here (`400`), not as a resolver `422`.
pub fn check_input_ref_uri(uri: &str) -> Result<(), InputRefUriError> {
    if uri.chars().any(char::is_control) {
        return Err(InputRefUriError::ControlCharacter);
    }
    if uri.chars().any(|ch| !matches!(u32::from(ch), 0x21..=0x7E)) {
        return Err(InputRefUriError::NotPrintableAscii);
    }
    let (scheme, rest) = uri.split_once("://").ok_or(InputRefUriError::Shape)?;
    if rest.is_empty() {
        return Err(InputRefUriError::Shape);
    }
    if !valid_scheme(scheme) {
        return Err(
            if scheme.bytes().any(|b| b.is_ascii_uppercase())
                && valid_scheme(&scheme.to_ascii_lowercase())
            {
                InputRefUriError::UppercaseScheme
            } else {
                InputRefUriError::Shape
            },
        );
    }
    if uri.contains(['?', '#', ';']) {
        return Err(InputRefUriError::ReservedDelimiter);
    }
    let lower = rest.to_ascii_lowercase();
    if lower.contains("%25") {
        return Err(InputRefUriError::ReservedDelimiter);
    }
    if rest.contains('\\') || ["%2e", "%2f", "%5c"].iter().any(|e| lower.contains(e)) {
        return Err(InputRefUriError::PathTraversal);
    }
    if rest
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(InputRefUriError::PathTraversal);
    }
    Ok(())
}

/// First path segment after `scheme://` (`s3://bucket/key` → `bucket`), for
/// resolvers whose URIs start with a project segment. `None` when the
/// segment is empty or the URI has no `://`.
pub fn uri_project_segment(uri: &str) -> Option<&str> {
    let (_, rest) = uri.split_once("://")?;
    let segment = rest.split(['/', '?', '#']).next()?;
    (!segment.is_empty()).then_some(segment)
}

/// Validation helper: the URI's first segment must equal
/// `claims.project_id()`. Missing segment → [`InputRefError::Rejected`];
/// different project → [`InputRefError::ScopeMismatch`].
pub fn check_project_segment(uri: &str, claims: &IsolationClaims) -> Result<(), InputRefError> {
    match uri_project_segment(uri) {
        None => Err(InputRefError::Rejected),
        Some(project) if project == claims.project_id() => Ok(()),
        Some(_) => Err(InputRefError::ScopeMismatch),
    }
}

/// Registration rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputRefRegistrationError {
    /// Scheme is not lowercase `[a-z][a-z0-9+.-]*`.
    InvalidScheme,
    /// Prefix is not `<valid scheme>://<non-empty>/` (must end with `/`).
    InvalidPrefix,
    /// The scheme or prefix is already registered.
    Duplicate,
    /// The scheme is already owned the other way: a prefix for a scheme that
    /// has a scheme resolver, or a scheme that already has prefixes.
    SchemeConflict,
    /// Startup finished; the registry no longer accepts registrations.
    Frozen,
}

impl std::fmt::Display for InputRefRegistrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidScheme => "input_ref scheme must match [a-z][a-z0-9+.-]*",
            Self::InvalidPrefix => "input_ref prefix must be <scheme>://<non-empty>/ ending in /",
            Self::Duplicate => "input_ref scheme or prefix is already registered",
            Self::SchemeConflict => {
                "input_ref scheme is already owned by a scheme resolver or by prefix resolvers"
            }
            Self::Frozen => "input_ref registry is frozen after startup",
        })
    }
}

impl std::error::Error for InputRefRegistrationError {}

type SharedResolver = Arc<dyn InputRefResolver>;

/// Scheme / prefix → resolver routing. Cheap to clone (shared tables; the
/// frozen flag is shared too).
#[derive(Clone)]
pub struct InputRefRegistry {
    by_scheme: Arc<DashMap<String, SharedResolver>>,
    /// Kept sorted by descending prefix length so the first match is the
    /// longest. Its write lock also serialises every registration.
    by_prefix: Arc<RwLock<Vec<(String, SharedResolver)>>>,
    frozen: Arc<AtomicBool>,
    timeout: Duration,
    max_bytes: usize,
}

impl Default for InputRefRegistry {
    fn default() -> Self {
        Self {
            by_scheme: Arc::default(),
            by_prefix: Arc::default(),
            frozen: Arc::default(),
            timeout: DEFAULT_INPUT_REF_TIMEOUT,
            max_bytes: DEFAULT_INPUT_REF_MAX_BYTES,
        }
    }
}

impl std::fmt::Debug for InputRefRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InputRefRegistry")
            .field("schemes", &self.schemes())
            .field("prefixes", &self.prefixes())
            .field("frozen", &self.is_frozen())
            .field("timeout", &self.timeout)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

fn valid_scheme(scheme: &str) -> bool {
    let mut chars = scheme.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))
}

/// `<valid scheme>://<at least one char, not starting with '/'>…/`.
fn prefix_scheme(prefix: &str) -> Option<&str> {
    let (scheme, rest) = prefix.split_once("://")?;
    let ok =
        valid_scheme(scheme) && rest.len() >= 2 && !rest.starts_with('/') && rest.ends_with('/');
    ok.then_some(scheme)
}

impl InputRefRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the per-resolution timeout (kernel-enforced).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Per-resolution timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Sets the content cap (kernel-enforced), clamped to
    /// `1..=`[`MAX_INPUT_REF_MAX_BYTES`].
    pub fn with_max_bytes(mut self, max_bytes: usize) -> Self {
        self.max_bytes = max_bytes.clamp(1, MAX_INPUT_REF_MAX_BYTES);
        self
    }

    /// Content cap passed to resolvers and enforced on their output.
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Stops accepting registrations (startup calls this once it is done).
    pub fn freeze(&self) {
        self.frozen.store(true, Ordering::SeqCst);
    }

    /// Whether [`Self::freeze`] was called.
    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::SeqCst)
    }

    fn write_prefixes(&self) -> std::sync::RwLockWriteGuard<'_, Vec<(String, SharedResolver)>> {
        self.by_prefix
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Routes every URI `<scheme>://…` to `resolver`. Rejected when the scheme
    /// already has a resolver or any prefix registration.
    pub fn register_scheme(
        &self,
        scheme: impl Into<String>,
        resolver: Arc<dyn InputRefResolver>,
    ) -> Result<(), InputRefRegistrationError> {
        let scheme = scheme.into();
        if !valid_scheme(&scheme) {
            return Err(InputRefRegistrationError::InvalidScheme);
        }
        let prefixes = self.write_prefixes();
        if self.is_frozen() {
            return Err(InputRefRegistrationError::Frozen);
        }
        if prefixes
            .iter()
            .any(|(prefix, _)| prefix_scheme(prefix) == Some(scheme.as_str()))
        {
            return Err(InputRefRegistrationError::SchemeConflict);
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
    /// `s3://bucket-a/`) to `resolver`. The prefix must end with `/`, so
    /// `s3://allowed/` never matches `s3://allowed2/…`. Rejected when the
    /// prefix's scheme already has a scheme resolver (a prefix can never
    /// shadow one, built-in included).
    pub fn register_prefix(
        &self,
        prefix: impl Into<String>,
        resolver: Arc<dyn InputRefResolver>,
    ) -> Result<(), InputRefRegistrationError> {
        let prefix = prefix.into();
        let Some(scheme) = prefix_scheme(&prefix) else {
            return Err(InputRefRegistrationError::InvalidPrefix);
        };
        let mut table = self.write_prefixes();
        if self.is_frozen() {
            return Err(InputRefRegistrationError::Frozen);
        }
        if self.by_scheme.contains_key(scheme) {
            return Err(InputRefRegistrationError::SchemeConflict);
        }
        if table.iter().any(|(existing, _)| *existing == prefix) {
            return Err(InputRefRegistrationError::Duplicate);
        }
        table.push((prefix, resolver));
        table.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
        Ok(())
    }

    /// Copies every registration of `other` into `self`. Entries that clash
    /// with an existing registration (or are rejected for any reason) are
    /// skipped and returned.
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

    /// Whether some registered resolver would handle `uri`.
    pub fn has_resolver_for(&self, uri: &str) -> bool {
        self.matching(uri).is_some()
    }

    /// Create-time check (no I/O): a resolver must match and accept `uri`
    /// for `claims`. Returns [`InputRefError::NoResolver`],
    /// [`InputRefError::ScopeMismatch`] or [`InputRefError::Rejected`].
    pub fn validate(&self, uri: &str, claims: &IsolationClaims) -> Result<(), InputRefError> {
        check_input_ref_uri(uri).map_err(|_| InputRefError::Rejected)?;
        let resolver = self.matching(uri).ok_or(InputRefError::NoResolver)?;
        resolver.validate(uri, claims).map_err(|error| match error {
            InputRefError::ScopeMismatch => InputRefError::ScopeMismatch,
            _ => InputRefError::Rejected,
        })
    }

    /// Resolves `uri` for `claims`: re-validates, calls the resolver under
    /// the kernel timeout (capped by `invocation_deadline`), and enforces
    /// [`Self::max_bytes`]. A URI failing [`check_input_ref_uri`] is
    /// rejected before routing.
    pub async fn resolve(
        &self,
        claims: &IsolationClaims,
        invocation_id: &str,
        uri: &str,
        invocation_deadline: Option<Instant>,
    ) -> Result<Vec<u8>, InputRefError> {
        check_input_ref_uri(uri).map_err(|_| InputRefError::Rejected)?;
        let resolver = self.matching(uri).ok_or(InputRefError::NoResolver)?;
        let scheme = Self::scheme_of(uri).ok_or(InputRefError::NoResolver)?;
        resolver.validate(uri, claims)?;
        let mut deadline = Instant::now() + self.timeout;
        if let Some(invocation_deadline) = invocation_deadline {
            deadline = deadline.min(invocation_deadline);
        }
        let call = resolver.resolve(InputRefRequest {
            uri,
            scheme,
            invocation_id,
            claims,
            max_bytes: self.max_bytes,
            deadline,
        });
        let bytes = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), call)
            .await
            .map_err(|_| InputRefError::Timeout)??;
        if bytes.len() > self.max_bytes {
            return Err(InputRefError::TooLarge);
        }
        Ok(bytes)
    }

    #[cfg(test)]
    pub(crate) async fn resolve_for_tests(
        &self,
        claims: &IsolationClaims,
        uri: &str,
    ) -> Result<Vec<u8>, InputRefError> {
        self.resolve(claims, "inv_test", uri, None).await
    }
}

/// Process-wide registry for embedders. Register external resolvers here, in
/// code, before calling `build_router`; startup copies them into the runtime
/// registry (after the built-in resolver, which therefore cannot be
/// replaced) and then freezes this registry, so a late registration fails
/// with [`InputRefRegistrationError::Frozen`] instead of being ignored.
pub fn deployment_input_ref_registry() -> &'static InputRefRegistry {
    static REGISTRY: OnceLock<InputRefRegistry> = OnceLock::new();
    REGISTRY.get_or_init(InputRefRegistry::new)
}

/// Lowercase hex SHA-256 of `bytes`.
pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Fetches `input_ref` for `claims`, checks the pinned digest (kernel-side,
/// never trusting the resolver) and requires UTF-8 text. Errors carry a
/// typed code and a fixed message; nothing from the URI, the resolver or the
/// content is echoed, and the log line carries only the invocation id,
/// scheme and failure class.
pub(crate) async fn fetch_and_verify_input_ref(
    registry: &InputRefRegistry,
    claims: &IsolationClaims,
    invocation_id: &str,
    uri: &str,
    expected_sha256: &str,
    invocation_deadline: Option<Instant>,
) -> Result<String, (&'static str, &'static str)> {
    let scheme = InputRefRegistry::scheme_of(uri).unwrap_or("");
    let fetch_failed = |class: &str| {
        tracing::warn!(invocation_id, scheme, class, "input_ref resolution failed");
        (
            INPUT_REF_FETCH_FAILED_ERROR_CODE,
            INPUT_REF_FETCH_FAILED_MESSAGE,
        )
    };
    let bytes = match registry
        .resolve(claims, invocation_id, uri, invocation_deadline)
        .await
    {
        Ok(bytes) => bytes,
        // Same fixed error and log line as a caller-pin mismatch below, so a
        // metadata digest failure is not distinguishable from a caller pin.
        Err(InputRefError::DigestMismatch) => {
            tracing::warn!(invocation_id, scheme, "input_ref digest mismatch");
            return Err((
                INPUT_DIGEST_MISMATCH_ERROR_CODE,
                INPUT_DIGEST_MISMATCH_MESSAGE,
            ));
        }
        Err(error) => return Err(fetch_failed(error.class())),
    };
    if sha256_hex(&bytes) != expected_sha256 {
        tracing::warn!(invocation_id, scheme, "input_ref digest mismatch");
        return Err((
            INPUT_DIGEST_MISMATCH_ERROR_CODE,
            INPUT_DIGEST_MISMATCH_MESSAGE,
        ));
    }
    String::from_utf8(bytes).map_err(|_| fetch_failed("not_utf8"))
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

/// Reads [`INPUT_REF_TIMEOUT_ENV`] (milliseconds, `1..=60000`). Missing or
/// invalid → [`DEFAULT_INPUT_REF_TIMEOUT`].
pub(crate) fn input_ref_timeout_from_vars(lookup: impl Fn(&str) -> Option<String>) -> Duration {
    let Some(raw) = lookup(INPUT_REF_TIMEOUT_ENV) else {
        return DEFAULT_INPUT_REF_TIMEOUT;
    };
    match raw.trim().parse::<u64>() {
        Ok(ms) if ms >= 1 && Duration::from_millis(ms) <= MAX_INPUT_REF_TIMEOUT => {
            Duration::from_millis(ms)
        }
        _ => {
            tracing::warn!(
                "{INPUT_REF_TIMEOUT_ENV} must be 1..=60000 ms; using the default {} ms",
                DEFAULT_INPUT_REF_TIMEOUT.as_millis()
            );
            DEFAULT_INPUT_REF_TIMEOUT
        }
    }
}

/// Reads [`INPUT_REF_MAX_BYTES_ENV`] (bytes, `1..=`[`MAX_INPUT_REF_MAX_BYTES`]).
/// Missing or invalid → [`DEFAULT_INPUT_REF_MAX_BYTES`].
pub(crate) fn input_ref_max_bytes_from_vars(lookup: impl Fn(&str) -> Option<String>) -> usize {
    let Some(raw) = lookup(INPUT_REF_MAX_BYTES_ENV) else {
        return DEFAULT_INPUT_REF_MAX_BYTES;
    };
    match raw.trim().parse::<usize>() {
        Ok(bytes) if (1..=MAX_INPUT_REF_MAX_BYTES).contains(&bytes) => bytes,
        _ => {
            tracing::warn!(
                "{INPUT_REF_MAX_BYTES_ENV} must be 1..={MAX_INPUT_REF_MAX_BYTES} bytes; using the default {DEFAULT_INPUT_REF_MAX_BYTES}"
            );
            DEFAULT_INPUT_REF_MAX_BYTES
        }
    }
}

/// Built-in resolver for `wao-artifact://<project_id>/<artifact-id>`.
///
/// Create-time `validate` requires the project segment to equal the caller's
/// `project_id`. `resolve` takes the caller's claims on [`InputRefRequest`]
/// and looks the id up through the claims-scoped metadata query, then reads
/// the bytes under the caller's tenant blob prefix. Only an `input_snapshot`
/// is returned. The bytes must match the SHA-256 stored on that metadata;
/// a mismatch is [`InputRefError::DigestMismatch`] (the kernel reports it
/// with the same error as a caller-pin mismatch). Anything else — malformed
/// id, another kind, another project's or tenant's artifact, a non-canonical
/// blob key, a missing blob — is [`InputRefError::NotFound`]. Reads only
/// local storage.
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

    /// Splits `wao-artifact://<project>/<lowercase hyphenated uuid>`.
    fn parse(uri: &str) -> Option<(&str, &str)> {
        let rest = uri.strip_prefix("wao-artifact://")?;
        let (project, id) = rest.split_once('/')?;
        if project.is_empty() {
            return None;
        }
        let parsed = uuid::Uuid::parse_str(id).ok()?;
        (parsed.hyphenated().to_string() == id).then_some((project, id))
    }
}

#[async_trait]
impl InputRefResolver for ArtifactInputRefResolver {
    fn validate(&self, uri: &str, claims: &IsolationClaims) -> Result<(), InputRefError> {
        let (project, _) = Self::parse(uri).ok_or(InputRefError::Rejected)?;
        if project != claims.project_id() {
            return Err(InputRefError::ScopeMismatch);
        }
        Ok(())
    }

    async fn resolve(&self, request: InputRefRequest<'_>) -> Result<Vec<u8>, InputRefError> {
        let (project, id) = Self::parse(request.uri).ok_or(InputRefError::NotFound)?;
        if project != request.claims.project_id() {
            return Err(InputRefError::NotFound);
        }
        let metadata = super::artifacts::load_artifact_metadata(&self.kg_store, request.claims, id)
            .map_err(|_| InputRefError::Unavailable)?
            .ok_or(InputRefError::NotFound)?;
        if metadata.kind != super::artifacts::ArtifactKind::InputSnapshot {
            return Err(InputRefError::NotFound);
        }
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
        if sha256_hex(&bytes) != metadata.sha256 {
            return Err(InputRefError::DigestMismatch);
        }
        Ok(bytes)
    }
}

/// Builds the runtime registry at startup: the built-in artifact resolver
/// when enabled and a blob store is configured, then every registration from
/// `embedders` (production passes [`deployment_input_ref_registry`]). The
/// caller freezes the result (and `embedders`) once wiring is done.
pub(crate) fn startup_input_ref_registry(
    kg_store: Arc<oxigraph::store::Store>,
    blob_store: Option<Arc<dyn BlobStore>>,
    artifacts_enabled: bool,
    embedders: &InputRefRegistry,
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
    for skipped in registry.extend_from(embedders) {
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
