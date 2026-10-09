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
use regex::{Regex, RegexSet};

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
    // `\n` / `\r` / `\t` / `\b` / `\f`, a JSON `\uXXXX` escape (the final hex
    // digit would otherwise count as an identifier character), or a
    // percent-encoded byte (`%20`, `%3D`). `sk-` inside `task-` / `risk-` /
    // `disk-` / `ask-` (or after `_` / `-`) is not a key prefix. Other prefix
    // rules match the issued prefix as a substring, so they do not need this
    // boundary. A lowercase hyphenated name is dropped later; see
    // [`is_kebab_sk_token`].
    ("sk_api_key", SK_API_KEY_PATTERN),
];

/// Left boundary plus the `sk-` token. Shared with [`SK_API_KEY_CAPTURE`].
const SK_API_KEY_PATTERN: &str = concat!(
    r"(?:^|[^A-Za-z0-9_-]|\\[nrtbf]|\\u[0-9A-Fa-f]{4}|%[0-9A-Fa-f]{2})",
    r"sk-(?:proj-|ant-)?[A-Za-z0-9_-]{20,}",
);

/// Same match as [`SK_API_KEY_PATTERN`], with the token in group 1 so a kebab
/// identifier can be told apart from a credential.
const SK_API_KEY_CAPTURE: &str = concat!(
    r"(?:^|[^A-Za-z0-9_-]|\\[nrtbf]|\\u[0-9A-Fa-f]{4}|%[0-9A-Fa-f]{2})",
    r"(sk-(?:proj-|ant-)?[A-Za-z0-9_-]{20,})",
);

/// Index of the `sk-` rule in [`RULES`].
const SK_API_KEY_RULE: usize = 6;

/// Index of the private-key rule in [`RULES`].
const PRIVATE_KEY_RULE: usize = 0;

static SECRET_SET: Lazy<RegexSet> = Lazy::new(|| {
    debug_assert_eq!(RULES[SK_API_KEY_RULE].0, "sk_api_key");
    RegexSet::new(RULES.iter().map(|(_, pattern)| *pattern))
        .expect("secret scan patterns must compile")
});

static SK_TOKEN: Lazy<Regex> =
    Lazy::new(|| Regex::new(SK_API_KEY_CAPTURE).expect("sk token pattern must compile"));

/// Rule indexes that match `text`, after kebab `sk-` names are dropped.
fn matching_rule_indexes(text: &str) -> Vec<usize> {
    let matched = SECRET_SET.matches(text);
    let suppress_sk = matched.matched(SK_API_KEY_RULE) && !sk_match_is_credential(text);
    matched
        .into_iter()
        .filter(|index| !suppress_sk || *index != SK_API_KEY_RULE)
        .collect()
}

/// True when some `sk-` match is a credential rather than a kebab identifier.
///
/// A digit requirement on every body would also drop the letter-only shapes
/// the regression samples use, so only a lowercase hyphenated name is ignored.
/// If the set matched but no token could be read, the hit stays (fail closed).
fn sk_match_is_credential(text: &str) -> bool {
    let mut saw_token = false;
    for captures in SK_TOKEN.captures_iter(text) {
        saw_token = true;
        let token = captures.get(1).map(|m| m.as_str()).unwrap_or("");
        if !is_kebab_sk_token(token) {
            return true;
        }
    }
    !saw_token
}

/// `sk-learn-classification-examples-v2` and similar kebab names.
///
/// Optional `proj-` / `ant-` prefixes are not part of the name. A segment of
/// 16 or more characters that contains a digit stays a credential, as does
/// any uppercase or underscore.
fn is_kebab_sk_token(token: &str) -> bool {
    let Some(rest) = token.strip_prefix("sk-") else {
        return false;
    };
    let body = rest
        .strip_prefix("proj-")
        .or_else(|| rest.strip_prefix("ant-"))
        .unwrap_or(rest);
    let segments: Vec<&str> = body.split('-').collect();
    if segments.len() < 3 {
        return false;
    }
    if segments.iter().any(|segment| {
        segment.is_empty()
            || !segment
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
    }) {
        return false;
    }
    if segments
        .iter()
        .any(|segment| segment.len() >= 16 && segment.chars().any(|ch| ch.is_ascii_digit()))
    {
        return false;
    }
    segments
        .iter()
        .filter(|segment| segment.chars().all(|ch| ch.is_ascii_lowercase()))
        .count()
        >= 2
}

/// Names of all rules that match `text`; diagnostics for tests only.
#[cfg(test)]
pub(crate) fn matched_rules(text: &str) -> Vec<&'static str> {
    matching_rule_indexes(text)
        .into_iter()
        .map(|index| RULES[index].0)
        .collect()
}

/// True when `text` embeds any recognizable credential value.
pub fn contains_plaintext_secret(text: &str) -> bool {
    !matching_rule_indexes(text).is_empty()
}

/// True when `text` contains PEM private-key armor.
pub fn contains_private_key(text: &str) -> bool {
    matching_rule_indexes(text).contains(&PRIVATE_KEY_RULE)
}

/// Number of non-private-key credential rules that match `text`.
pub fn credential_token_hits(text: &str) -> usize {
    matching_rule_indexes(text)
        .into_iter()
        .filter(|index| *index != PRIVATE_KEY_RULE)
        .count()
}

#[cfg(test)]
#[path = "secret_scan_tests.rs"]
pub(crate) mod tests;
