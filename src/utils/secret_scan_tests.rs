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
        // Exactly-one-rule doubles as a mutation check: deleting or weakening
        // any rule lets its sample through, because no other rule matches it.
        assert_eq!(matched_rules(&sample), vec![name], "sample for {name}");
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
