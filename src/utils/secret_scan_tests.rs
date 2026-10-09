//! Secret-scan regression tests. Samples are obviously fake (`FAKE…`,
//! sequential hex) but have the real shape; prefixes are joined at runtime so
//! this file never contains a contiguous token literal.

use super::*;

/// `len` characters of an obviously fake token body (`FAKEFAKE…`).
fn fake(len: usize) -> String {
    "FAKE".chars().cycle().take(len).collect()
}

fn cat(parts: &[&str]) -> String {
    parts.concat()
}

/// Ordinary text that the old substring guard blocked or that a naive
/// case-insensitive scan flags (`akia` in `Slovakia`).
fn ordinary_samples() -> Vec<String> {
    vec![
        r#"{"task-id": "task-20261008-0001", "task-kind": "review-pipeline-stage"}"#.to_string(),
        r#"{"risk-level": "high", "risk-assessment-for-quarterly-report": 0.82}"#.to_string(),
        r#"{"disk-usage": "73%", "disk-cleanup-schedule-weekly-default": true}"#.to_string(),
        r#"{"steps": ["ask-user-for-confirmation-before-continuing"], "ask-price": 1}"#.to_string(),
        "Shipping to Slovakia and Akiak; the AKIA prefix alone is not a key".to_string(),
        cat(&["-----", "BEGIN CERTIFICATE-----\nMIIB", &fake(60)]),
        "export AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}".to_string(),
        "export TOKEN=\"$TOKEN_FROM_ENV\"; client-sk-integration-handler-v2".to_string(),
        // Kebab identifiers of 20+ characters are names, not keys.
        cat(&["s", "k-learn-classification-examples-v2"]),
        cat(&["note ", "s", "k-my-long-running-service-name end"]),
        // Escaped newlines/tabs and percent-encoding next to ordinary words.
        r#"{"log": "line1\nask-user-for-confirmation-before-continuing\ttask-risk-review-pipeline%20disk-cleanup-schedule-weekly"}"#
            .to_string(),
    ]
}

/// Leaks found in review of the first real-format version: `main` rejected
/// them, that version let them through. Each must stay rejected.
pub(crate) fn review_regression_samples() -> Vec<(&'static str, String)> {
    let sk = cat(&["s", "k-proj-", &fake(48)]);
    let aws = fake(40);
    vec![
        // `\n` / `\t` / `\r` escapes in JSON or log text.
        ("sk_api_key", cat(&[r#"{"content":"key:\n"#, &sk, "\"}"])),
        ("sk_api_key", cat(&[r"\t", &sk])),
        ("sk_api_key", cat(&[r"line\r", &sk])),
        // Percent-encoded separators.
        ("sk_api_key", cat(&["Authorization: Bearer%20", &sk])),
        ("sk_api_key", cat(&["?key%3D", &sk])),
        ("sk_api_key", cat(&["?key%3d", &sk])),
        // JSON nested in a JSON string (escaped quotes).
        (
            "aws_secret_access_key",
            cat(&[r#"{\"aws_secret_"#, r#"access_key\": \""#, &aws, r#"\"}"#]),
        ),
        // PHP / Ruby hash syntax.
        (
            "aws_secret_access_key",
            cat(&["'aws_secret_", "access_key' => '", &aws, "'"]),
        ),
        // Lowercase hyphenated keys with a digit in a segment. A letters-only
        // name exemption must not let these through.
        (
            "sk_api_key",
            cat(&["s", "k-q7xk2mzp9w-abtrvnqe-hzkwplms-x83kd0q2a1"]),
        ),
        (
            "sk_api_key",
            cat(&["s", "k-proj-abcdefgh-ijklmnop-q1w2e3r4t5y6u7i"]),
        ),
    ]
}

/// Must-block samples that pin rule details a weaker rule would miss. Each
/// comment names the weakening it guards against; see the PR for the
/// mutation runs.
fn rule_detail_samples() -> Vec<(&'static str, String)> {
    let aws = fake(40);
    vec![
        // PEM without an algorithm prefix (PKCS#8), and a digit in the label.
        (
            "private_key",
            cat(&["-----", "BEGIN ", "PRIVATE KEY-----\n", &fake(64)]),
        ),
        (
            "private_key",
            cat(&["-----", "BEGIN ED25519 ", "PRIVATE KEY-----"]),
        ),
        // Slack: every accepted type, not only `xoxb-`.
        ("slack_token", cat(&["xo", "xp-", &fake(24)])),
        ("slack_token", cat(&["xo", "xa-", &fake(24)])),
        ("slack_token", cat(&["xo", "xr-", &fake(24)])),
        ("slack_token", cat(&["xo", "xs-", &fake(24)])),
        // Slack: length threshold stays at 10.
        ("slack_token", cat(&["xo", "xb-", &fake(12)])),
        // sk: length threshold stays at 20 (also 32-hex compatible keys).
        ("sk_api_key", cat(&[" s", "k-", &fake(24)])),
        (
            "sk_api_key",
            cat(&["=s", "k-", "0123456789abcdef0123456789abcdef"]),
        ),
        // sk: key at the very start of the text.
        ("sk_api_key", cat(&["s", "k-", &fake(48)])),
        // AWS: `:` separator, double and single quotes.
        (
            "aws_secret_access_key",
            cat(&["aws_secret_", "access_key: ", &aws]),
        ),
        (
            "aws_secret_access_key",
            cat(&["{\"aws_secret_", "access_key\": \"", &aws, "\"}"]),
        ),
        (
            "aws_secret_access_key",
            cat(&["aws_secret_", "access_key='", &aws, "'"]),
        ),
        // AWS: name is case-insensitive.
        (
            "aws_secret_access_key",
            cat(&["AWS_SECRET_", "ACCESS_KEY=", &aws]),
        ),
    ]
}

/// One real-shaped sample per rule (the `sk` rule gets three shapes).
fn real_samples() -> Vec<(&'static str, String)> {
    vec![
        (
            "private_key",
            cat(&["-----", "BEGIN OPENSSH ", "PRIVATE KEY-----\n", &fake(64)]),
        ),
        (
            "aws_access_key_id",
            cat(&["{\"id\":\"", "AK", "IA", &fake(16), "\"}"]),
        ),
        (
            "aws_secret_access_key",
            cat(&["aws_secret_", "access_key = ", &fake(40)]),
        ),
        (
            "github_classic_pat",
            cat(&["export T=", "gh", "p_", &fake(36)]),
        ),
        (
            "github_fine_grained_pat",
            cat(&["token: ", "github_", "pat_", &fake(22), "_", &fake(59)]),
        ),
        (
            "slack_token",
            cat(&["Bearer ", "xo", "xb-", "1234567890-", &fake(24)]),
        ),
        (
            "sk_api_key",
            cat(&["{\"k\": \"", "s", "k-proj-", &fake(48), "\"}"]),
        ),
        ("sk_api_key", cat(&["key=", "s", "k-ant-api03-", &fake(80)])),
        (
            "sk_api_key",
            cat(&[
                "Authorization: Bearer ",
                "s",
                "k-acme-",
                &"0123456789abcdef".repeat(4),
            ]),
        ),
    ]
}

#[test]
fn ordinary_text_passes() {
    for sample in ordinary_samples() {
        assert_eq!(
            matched_rules(&sample),
            Vec::<&str>::new(),
            "must not block: {sample}"
        );
        assert!(!contains_plaintext_secret(&sample));
    }
}

#[test]
fn every_real_shape_is_caught_by_exactly_its_rule() {
    let mut covered: Vec<&str> = real_samples().iter().map(|(name, _)| *name).collect();
    covered.dedup();
    let all: Vec<&str> = RULES.iter().map(|(name, _)| *name).collect();
    assert_eq!(covered, all, "every rule needs a positive sample");
    for (name, sample) in real_samples() {
        assert!(contains_plaintext_secret(&sample), "must block: {name}");
        // No other rule matches the sample, so deleting this rule turns the
        // test red. (Weakening is covered by `rule_detail_samples`.)
        assert_eq!(matched_rules(&sample), vec![name], "sample for {name}");
    }
}

#[test]
fn review_regressions_stay_rejected() {
    for (name, sample) in review_regression_samples() {
        assert_eq!(matched_rules(&sample), vec![name], "must block: {sample}");
    }
}

#[test]
fn rule_details_are_pinned() {
    for (name, sample) in rule_detail_samples() {
        assert_eq!(matched_rules(&sample), vec![name], "must block: {sample}");
    }
}

#[test]
fn near_miss_mutations_pass() {
    let near_misses = [
        // One token character short.
        cat(&["gh", "p_", &fake(35)]),
        cat(&["github_", "pat_", &fake(81)]),
        cat(&["xo", "xb-", &fake(9)]),
        cat(&["AK", "IA", &fake(15)]),
        cat(&["aws_secret_", "access_key=", &fake(39)]),
        cat(&[" s", "k-", &fake(19)]),
        // Prefix glued to an identifier, including `-` and `_`.
        cat(&["ta", "sk-", &fake(40)]),
        cat(&["foo-", "s", "k-", &fake(40)]),
        cat(&["my_", "s", "k-", &fake(40)]),
        // Case differs from the issued format.
        cat(&["S", "K-", &fake(40)]),
        cat(&["G", "HP_", &fake(36)]),
        cat(&["XO", "XB-", &fake(20)]),
        cat(&["ak", "ia", &fake(16)]),
        cat(&["-----", "begin rsa private key-----"]),
        // Value referenced, not embedded.
        cat(&["aws_secret_", "access_key: ${{ secrets.AWS_SECRET }}"]),
    ];
    for sample in near_misses {
        assert!(
            !contains_plaintext_secret(&sample),
            "must not block: {sample}"
        );
    }
}

/// Raw JSON/text escapes that sit immediately left of an `sk-` key.
/// `\uXXXX` ends in a hex digit, so a boundary of only `[^A-Za-z0-9_-]` misses
/// the key. `\b` and `\f` were outside the escaped-whitespace class.
#[test]
fn json_unicode_and_control_escapes_are_sk_boundaries() {
    let sk = cat(&["s", "k-proj-", &fake(48)]);
    let probes = [
        // Full-width colon, as emitted when non-ASCII is escaped.
        cat(&[r"API Key\uff1a", &sk]),
        cat(&[r"\u3000", &sk]),
        cat(&[r"\b", &sk]),
        cat(&[r"\f", &sk]),
        cat(&[r"line\b", &sk]),
        cat(&[r"line\f", &sk]),
    ];
    for sample in probes {
        assert_eq!(
            matched_rules(&sample),
            vec!["sk_api_key"],
            "must block: {sample}"
        );
    }
    // The escape is a boundary only when `sk-` follows it. `ask-` does not.
    let ordinary = cat(&[
        r"\u3000ask-user-for-confirmation-before-continuing",
        r" \bask-user-for-confirmation-before-continuing",
    ]);
    assert_eq!(matched_rules(&ordinary), Vec::<&str>::new());
}

/// A name is exempt only when every segment after `sk-` is lowercase letters,
/// or the final segment is `v` plus digits. Digit-bearing keys stay rejected.
#[test]
fn kebab_identifiers_are_not_secrets_and_mutations_stay_rejected() {
    assert_eq!(RULES[SK_API_KEY_RULE].0, "sk_api_key");
    let samples = [
        cat(&["s", "k-learn-classification-examples-v2"]),
        cat(&["s", "k-my-long-running-service-name"]),
        cat(&["s", "k-ant-learn-classification-examples-v2"]),
    ];
    for sample in &samples {
        assert_eq!(
            matched_rules(sample),
            Vec::<&str>::new(),
            "kebab name must pass: {sample}"
        );
        assert!(!contains_plaintext_secret(sample));
    }

    let real = cat(&["s", "k-proj-", &fake(48)]);
    let mixed = format!("{} {real}", samples[0]);
    assert_eq!(matched_rules(&mixed), vec!["sk_api_key"]);

    let long_hex = "0123456789abcdef".repeat(2);
    let mutations = [
        // Hyphens removed: letter-only body, same shape as the regression samples.
        cat(&["s", "k-learnclassificationexamplesv2"]),
        // Uppercase is not a kebab name.
        cat(&["s", "k-Learn-classification-examples-v2"]),
        // A long digit segment is a credential body, not a word.
        cat(&["s", "k-learn-classification-", &long_hex]),
        // Lowercase keys with a digit in a segment. Main blocked both.
        cat(&["s", "k-q7xk2mzp9w-abtrvnqe-hzkwplms-x83kd0q2a1"]),
        cat(&["s", "k-proj-abcdefgh-ijklmnop-q1w2e3r4t5y6u7i"]),
    ];
    for sample in &mutations {
        assert_eq!(
            matched_rules(sample),
            vec!["sk_api_key"],
            "mutation must block: {sample}"
        );
    }
}

#[test]
fn private_key_and_token_helpers_split_hard_and_soft_hits() {
    let key = cat(&["-----", "BEGIN RSA ", "PRIVATE KEY-----"]);
    let pgp = cat(&["-----", "BEGIN PGP ", "PRIVATE KEY BLOCK-----"]);
    let token = cat(&["gh", "p_", &fake(36)]);
    assert!(contains_private_key(&key));
    assert!(contains_private_key(&pgp));
    assert_eq!(credential_token_hits(&key), 0);
    assert!(!contains_private_key(&token));
    assert_eq!(credential_token_hits(&token), 1);
    assert_eq!(credential_token_hits("Slovakia"), 0);
}
