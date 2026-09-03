//! Value objects for semantic memory recall inputs.
//!
//! A task IRI identifies the task being handled; it is not semantic text and
//! must never be sent to an embedding service as a recall query. Scheduler
//! wiring intentionally remains a follow-up change.

use std::fmt;

/// Identifies the task associated with a recall request.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaskIri(String);

impl TaskIri {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskIri {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Text suitable for generating a semantic embedding.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SemanticQuery(String);

impl SemanticQuery {
    /// Creates a query only from non-empty, non-IRI semantic text.
    pub fn try_new(value: impl AsRef<str>) -> Result<Self, SemanticQueryError> {
        let value = value.as_ref().trim();
        if value.is_empty() {
            return Err(SemanticQueryError::Empty);
        }
        if looks_like_iri(value) {
            return Err(SemanticQueryError::Iri);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SemanticQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticQueryError {
    Empty,
    Iri,
}

impl fmt::Display for SemanticQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("semantic query must not be empty"),
            Self::Iri => formatter.write_str("an IRI cannot be used as a semantic query"),
        }
    }
}

impl std::error::Error for SemanticQueryError {}

/// Returns whether `value` starts with an RFC 3986 scheme, and is therefore
/// an IRI rather than natural-language semantic text.
fn looks_like_iri(value: &str) -> bool {
    let Some((scheme, _)) = value.split_once(':') else {
        return false;
    };

    !scheme.is_empty()
        && scheme.bytes().enumerate().all(|(index, byte)| match byte {
            b'a'..=b'z' | b'A'..=b'Z' => true,
            b'0'..=b'9' | b'+' | b'-' | b'.' => index > 0,
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_iri_cannot_be_used_as_an_embedding_query() {
        let task_iri = TaskIri::new("iri://task/repair-battery");

        let result = SemanticQuery::try_new(task_iri.as_str());

        assert_eq!(result, Err(SemanticQueryError::Iri));
    }

    #[test]
    fn natural_language_is_a_valid_semantic_query() {
        let query = SemanticQuery::try_new("how to repair a battery").unwrap();

        assert_eq!(query.as_str(), "how to repair a battery");
    }
}
