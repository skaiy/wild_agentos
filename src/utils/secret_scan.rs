//! Shared plaintext-credential detector.
//!
//! Used by artifact uploads (hard reject) and the skill pipeline's file scan
//! (private keys fail, other credential formats warn). Each rule matches a
//! real credential *format* rather than a bare substring: fixed prefixes,
//! enough token characters, and letter case as issued. Ordinary text such as
//! `task-`, `risk-`, `disk-`, `ask-` or `Slovakia` therefore never matches.
//! Referencing a credential by name (`KEY=${KEY}`, `"$TOKEN_FROM_ENV"`) is
//! allowed; embedding its value is not.
//!
//! Callers must never echo matched text. [`matched_rules`] returns rule names
//! for diagnostics and tests only.

use once_cell::sync::Lazy;
use regex::RegexSet;

/// `(name, pattern)`; index order is the [`RegexSet`] match index.
const RULES: &[(&str, &str)] = &[
    // PEM private-key armor only (PKCS#8 `BEGIN PRIVATE KEY`, RSA, EC,
    // OPENSSH, ED25519, PGP, ...); certificates and public keys pass.
    (
        "private_key",
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----",
    ),
    // AWS access key id.
    ("aws_access_key_id", r"AKIA[0-9A-Z]{16}"),
    // AWS secret access key assigned to its well-known name with `=`, `:` or
    // `=>`, optionally quoted, including JSON-in-JSON escaped quotes
    // (`\"key\": \"…\"`). The name alone, or a reference such as
    // `=${AWS_SECRET_ACCESS_KEY}`, does not match.
    (
        "aws_secret_access_key",
        r#"(?i:aws_secret_access_key)\\?["']?\s*(?:=>|[:=])\s*\\?["']?[A-Za-z0-9/+=]{40}"#,
    ),
    // GitHub classic personal access token.
    ("github_classic_pat", r"ghp_[A-Za-z0-9]{36}"),
    // GitHub fine-grained personal access token.
    ("github_fine_grained_pat", r"github_pat_[A-Za-z0-9_]{82}"),
    // Slack bot / app / user / refresh / session tokens.
    ("slack_token", r"xox[baprs]-[A-Za-z0-9-]{10,}"),
    // `sk-` API keys, including `sk-proj-…`, `sk-ant-…` and `sk-<slug>-<hex>`.
    // The `regex` crate has no look-behind, so the left boundary is a
    // non-capturing group: start of text, a non-token character, an escaped
    // `\n` / `\r` / `\t` (JSON or log text), or a percent-encoded byte
    // (`%20`, `%3D`). `sk-` inside `task-` / `risk-` / `disk-` / `ask-` (or
    // after `_` / `-`) is not a key prefix.
    (
        "sk_api_key",
        r"(?:^|[^A-Za-z0-9_-]|\\[nrt]|%[0-9A-Fa-f]{2})sk-(?:proj-|ant-)?[A-Za-z0-9_-]{20,}",
    ),
];

/// Index of the private-key rule in [`RULES`].
const PRIVATE_KEY_RULE: usize = 0;

static SECRET_SET: Lazy<RegexSet> = Lazy::new(|| {
    RegexSet::new(RULES.iter().map(|(_, pattern)| *pattern))
        .expect("secret scan patterns must compile")
});

/// Names of all rules that match `text`; diagnostics for tests only.
#[cfg(test)]
pub(crate) fn matched_rules(text: &str) -> Vec<&'static str> {
    SECRET_SET
        .matches(text)
        .into_iter()
        .map(|index| RULES[index].0)
        .collect()
}

/// True when `text` embeds any recognizable credential value.
pub fn contains_plaintext_secret(text: &str) -> bool {
    SECRET_SET.is_match(text)
}

/// True when `text` contains PEM private-key armor.
pub fn contains_private_key(text: &str) -> bool {
    SECRET_SET.matches(text).matched(PRIVATE_KEY_RULE)
}

/// Number of non-private-key credential rules that match `text`.
pub fn credential_token_hits(text: &str) -> usize {
    SECRET_SET
        .matches(text)
        .into_iter()
        .filter(|index| *index != PRIVATE_KEY_RULE)
        .count()
}

#[cfg(test)]
#[path = "secret_scan_tests.rs"]
pub(crate) mod tests;
