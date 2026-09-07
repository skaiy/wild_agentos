//! Kernel contracts for tenant isolation.
//!
//! [`IsolationClaims`] represents identity attributes that an authentication
//! boundary has already verified. This module is not an identity provider: it
//! neither reads requests nor verifies credentials. Consequently, its only
//! public claims constructor is [`IsolationClaims::from_verified`].
//!
//! The naming functions mint future, tenant-scoped names only. Minting is not
//! a migration of existing data: this module does not create directories or
//! access Oxigraph, Hyperspace, MinIO, or any other storage backend.
//!
//! ```compile_fail
//! use wild_agent_os_core::isolation::IsolationClaims;
//!
//! let body = r#"{"tenant_id":"acme"}"#;
//! let _claims = IsolationClaims::from_body(body);
//! ```
//!
//! ```compile_fail
//! use axum::http::HeaderMap;
//! use wild_agent_os_core::isolation::IsolationClaims;
//!
//! let mut headers = HeaderMap::new();
//! headers.insert("X-Identity", "eyJ0ZW5hbnRfaWQiOiJhY21lIn0=".parse().unwrap());
//! let _claims = IsolationClaims::from_headers(&headers);
//! ```

use std::path::PathBuf;

use thiserror::Error;

pub mod diagnose;
pub mod migrate;

/// Verified tenant, project, and actor identity for a kernel operation.
///
/// Fields intentionally remain private so callers cannot create or alter
/// claims except through [`Self::from_verified`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IsolationClaims {
    tenant_id: String,
    project_id: String,
    actor_id: String,
}

/// Reasons a tenant isolation identifier or minted name was rejected.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum IsolationError {
    #[error("{kind} must not be empty")]
    EmptyIdentifier { kind: &'static str },
    #[error("{kind} contains an unsafe character or path segment: {value:?}")]
    UnsafeIdentifier { kind: &'static str, value: String },
}

impl IsolationClaims {
    /// Builds claims from values that an authentication boundary has verified.
    ///
    /// This is intentionally the sole public constructor. Verification itself
    /// belongs to an upstream authentication boundary, not this kernel module.
    pub fn from_verified(
        tenant_id: impl Into<String>,
        project_id: impl Into<String>,
        actor_id: impl Into<String>,
    ) -> Result<Self, IsolationError> {
        let tenant_id = tenant_id.into();
        let project_id = project_id.into();
        let actor_id = actor_id.into();

        assert_tenant_id(&tenant_id)?;
        assert_project_id(&project_id)?;
        assert_actor_id(&actor_id)?;

        Ok(Self {
            tenant_id,
            project_id,
            actor_id,
        })
    }

    /// Returns the verified tenant identifier.
    pub fn tenant_id(&self) -> &str {
        &self.tenant_id
    }

    /// Returns the verified project identifier.
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Returns the verified actor identifier.
    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    /// Mints the tenant/project graph IRI without accessing a graph store.
    pub fn graph_iri(&self) -> Result<String, IsolationError> {
        self.assert_scope()?;
        Ok(format!("graph://{}/{}", self.tenant_id, self.project_id))
    }

    /// Mints the tenant object-key prefix without accessing object storage.
    pub fn object_key_prefix(&self) -> Result<String, IsolationError> {
        assert_tenant_id(&self.tenant_id)?;
        Ok(format!("{}/", self.tenant_id))
    }

    /// Mints the tenant/project vector namespace without accessing a vector store.
    pub fn vector_namespace(&self) -> Result<String, IsolationError> {
        self.assert_scope()?;
        Ok(format!("vector://{}/{}", self.tenant_id, self.project_id))
    }

    /// Mints the tenant L0 path without creating it.
    pub fn l0_path(&self) -> Result<PathBuf, IsolationError> {
        assert_tenant_id(&self.tenant_id)?;
        Ok(PathBuf::from("/data/l0").join(&self.tenant_id))
    }

    fn assert_scope(&self) -> Result<(), IsolationError> {
        assert_tenant_id(&self.tenant_id)?;
        assert_project_id(&self.project_id)
    }
}

/// Fails closed unless `tenant_id` is a safe, single identifier segment.
pub fn assert_tenant_id(tenant_id: &str) -> Result<(), IsolationError> {
    assert_identifier("tenant_id", tenant_id)
}

/// Fails closed unless `project_id` is a safe, single identifier segment.
pub fn assert_project_id(project_id: &str) -> Result<(), IsolationError> {
    assert_identifier("project_id", project_id)
}

/// Fails closed unless `actor_id` is a safe, single identifier segment.
pub fn assert_actor_id(actor_id: &str) -> Result<(), IsolationError> {
    assert_identifier("actor_id", actor_id)
}

fn assert_identifier(kind: &'static str, value: &str) -> Result<(), IsolationError> {
    if value.is_empty() {
        return Err(IsolationError::EmptyIdentifier { kind });
    }

    if value == "." || value == ".." || !value.bytes().all(is_safe_identifier_byte) {
        return Err(IsolationError::UnsafeIdentifier {
            kind,
            value: value.to_owned(),
        });
    }

    Ok(())
}

fn is_safe_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verified_claims_mint_canonical_tenant_names() {
        let claims = IsolationClaims::from_verified("acme", "research_1", "agent-7").unwrap();

        assert_eq!(claims.tenant_id(), "acme");
        assert_eq!(claims.project_id(), "research_1");
        assert_eq!(claims.actor_id(), "agent-7");
        assert_eq!(claims.graph_iri().unwrap(), "graph://acme/research_1");
        assert_eq!(claims.object_key_prefix().unwrap(), "acme/");
        assert_eq!(
            claims.vector_namespace().unwrap(),
            "vector://acme/research_1"
        );
        assert_eq!(claims.l0_path().unwrap(), PathBuf::from("/data/l0/acme"));
    }

    #[test]
    fn identifiers_fail_closed_for_empty_paths_and_untrusted_characters() {
        for invalid in ["", ".", "..", "a/b", r"a\b", "a b", "a:b", "a?b", "å"] {
            assert!(
                assert_tenant_id(invalid).is_err(),
                "{invalid:?} accepted as tenant"
            );
            assert!(
                assert_project_id(invalid).is_err(),
                "{invalid:?} accepted as project"
            );
            assert!(
                IsolationClaims::from_verified(invalid, "project", "actor").is_err(),
                "{invalid:?} accepted in claims"
            );
        }
    }

    #[test]
    fn safe_identifier_characters_are_accepted() {
        let claims = IsolationClaims::from_verified("Acme_01", "project-2", "actor_3").unwrap();
        assert_eq!(claims.graph_iri().unwrap(), "graph://Acme_01/project-2");
    }
}
