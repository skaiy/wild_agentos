//! Plaintext-credential guard for artifact uploads.
//!
//! Each rule matches a real credential *format*, not a bare substring: token
//! prefixes must start at an ASCII word boundary and be followed by enough
//! token characters, and letter case follows the real format. Ordinary text
//! such as `task-`, `risk-`, `disk-` or `ask-` therefore never matches, while
//! every recognizable credential value is still rejected before persistence.
//! Referencing a credential by name (for example `KEY=$FROM_ENV`) is allowed;
//! embedding its value is not.

use once_cell::sync::Lazy;
use regex::Regex;

/// Prefix boundary: start of input or a character that cannot be part of an
/// ASCII identifier. Unlike Unicode `\b`, CJK text directly before a token
/// still counts as a boundary.
const B: &str = r"(?:^|[^A-Za-z0-9_])";
/// Suffix boundary for fixed-length formats.
const E: &str = r"(?:$|[^A-Za-z0-9_])";

struct SecretRule {
    name: &'static str,
    pattern: Regex,
}

static RULES: Lazy<Vec<SecretRule>> =
    Lazy::new(|| {
        let rule = |name: &'static str, pattern: String| SecretRule {
            name,
            pattern: Regex::new(&pattern).expect("secret guard pattern must compile"),
        };
        vec![
        // Any PEM armor header (same coverage as before).
        rule("pem_armor_header", r"-----BEGIN [A-Z0-9][A-Z0-9 ]*-----".to_string()),
        // Private-key header or footer, so a block truncated before or after
        // its `BEGIN` line is still caught.
        rule(
            "pem_private_key_marker",
            r"PRIVATE KEY(?: BLOCK)?-----".to_string(),
        ),
        // AWS secret access key assigned to its well-known name (any case,
        // `_`/`-` separators, or the `SecretAccessKey` JSON field).
        rule(
            "aws_secret_access_key",
            r#"(?i:(?:aws[_-]?)?secret[_-]?access[_-]?key)["']?\s*[:=]\s*["']?[A-Za-z0-9/+=]{40}"#
                .to_string(),
        ),
        // AWS access key id (long-term `AKIA`, temporary `ASIA`).
        rule("aws_access_key_id", format!("{B}(?:AKIA|ASIA)[0-9A-Z]{{16}}{E}")),
        // GitHub fine-grained personal access token.
        rule("github_fine_grained_pat", format!("{B}github_pat_[A-Za-z0-9_]{{22,}}")),
        // GitHub classic PAT, OAuth, user-to-server, server-to-server, refresh.
        rule("github_token", format!("{B}gh[pousr]_[A-Za-z0-9]{{36,}}")),
        // Slack bot, user, app-level legacy, refresh and config tokens.
        rule("slack_token", format!("{B}xox[abposr]-[A-Za-z0-9-]{{10,}}")),
        // `sk-` API keys (OpenAI legacy/project, Anthropic, and compatible
        // gateways), including the `sk-proj-` / `sk-ant-` shapes.
        rule("sk_api_key", format!("{B}sk-[A-Za-z0-9_-]{{20,}}")),
    ]
    });

/// Returns the names of all rules that match `bytes`. Rule names are safe to
/// return to callers; matched values never leave this function.
pub(super) fn matching_secret_rules(bytes: &[u8]) -> Vec<&'static str> {
    let text = String::from_utf8_lossy(bytes);
    RULES
        .iter()
        .filter(|rule| rule.pattern.is_match(&text))
        .map(|rule| rule.name)
        .collect()
}

/// Blocks recognizable credential material before it can be persisted.
#[cfg(test)]
pub(super) fn contains_plaintext_secret(bytes: &[u8]) -> bool {
    !matching_secret_rules(bytes).is_empty()
}

#[cfg(test)]
#[path = "secret_guard_tests.rs"]
mod tests;
