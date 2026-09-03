//! Environment handling for untrusted child processes.
//!
//! Bash tools may run arbitrary commands, so they must not inherit credentials
//! from the AgentOS process. This module removes variables whose names are
//! conventionally used for secrets before creating a child process.

use std::ffi::{OsStr, OsString};

const SENSITIVE_EXACT_KEYS: &[&str] = &[
    "AGENTOS_JWT_SECRET",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
];

const SENSITIVE_PREFIXES: &[&str] = &["AWS_"];

const SENSITIVE_FRAGMENTS: &[&str] = &[
    "API_KEY",
    "APIKEY",
    "ACCESS_KEY",
    "SECRET",
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "PRIVATE_KEY",
    "CREDENTIAL",
    "AUTHORIZATION",
    "COOKIE",
];

/// Returns whether an environment variable name may contain a credential.
#[must_use]
pub fn is_sensitive_environment_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    SENSITIVE_EXACT_KEYS.contains(&key.as_str())
        || SENSITIVE_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
        || SENSITIVE_FRAGMENTS
            .iter()
            .any(|fragment| key.contains(fragment))
}

/// Removes secret-like entries from an environment intended for a child process.
///
/// Non-Unicode names are omitted conservatively because they cannot be safely
/// classified against the denylist.
pub fn sanitize_environment<I>(vars: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    vars.into_iter()
        .filter(|(key, _)| {
            key.to_str()
                .is_some_and(|key| !is_sensitive_environment_key(key))
        })
        .collect()
}

/// Captures the current process environment without values named like secrets.
#[must_use]
pub fn sanitized_child_environment() -> Vec<(OsString, OsString)> {
    sanitize_environment(std::env::vars_os())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(entries: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        entries
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value)))
            .collect()
    }

    #[test]
    fn sanitization_keeps_regular_values_and_removes_secret_like_keys() {
        let sanitized = sanitize_environment(vars(&[
            ("PATH", "/usr/bin"),
            ("WORKSPACE", "/workspace"),
            ("AWS_REGION", "us-east-1"),
            ("AGENTOS_JWT_SECRET", "jwt-secret"),
            ("GITHUB_TOKEN", "token"),
            ("SERVICE_API_KEY", "api-key"),
            ("DATABASE_PASSWORD", "password"),
        ]));
        let keys: Vec<&OsStr> = sanitized.iter().map(|(key, _)| key.as_os_str()).collect();

        assert!(keys.contains(&OsStr::new("PATH")));
        assert!(keys.contains(&OsStr::new("WORKSPACE")));
        assert!(!keys.contains(&OsStr::new("AWS_REGION")));
        assert!(!keys.contains(&OsStr::new("AGENTOS_JWT_SECRET")));
        assert!(!keys.contains(&OsStr::new("GITHUB_TOKEN")));
        assert!(!keys.contains(&OsStr::new("SERVICE_API_KEY")));
        assert!(!keys.contains(&OsStr::new("DATABASE_PASSWORD")));
    }
}
