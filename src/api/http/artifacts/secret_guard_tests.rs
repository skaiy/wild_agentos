//! Secret-guard regression tests. Credential-shaped samples are assembled at
//! runtime from fragments so this source file never contains a literal token.

use super::*;

const ALNUM: &[u8] = b"Ab3dE5gH7jK9mN1pQ2rS4tU6vW8xY0zC";

/// Deterministic token body of `len` mixed-case alphanumeric characters.
fn body(len: usize) -> String {
    (0..len)
        .map(|i| ALNUM[(i * 7 + 3) % ALNUM.len()] as char)
        .collect()
}

fn cat(parts: &[&str]) -> String {
    parts.concat()
}

fn upper_body(len: usize) -> String {
    body(len).to_ascii_uppercase()
}

/// One positive sample per rule, chosen so that it matches *only* that rule.
fn real_samples() -> Vec<(&'static str, String)> {
    vec![
        (
            // Non-key PEM armor is only caught by the header rule.
            "pem_armor_header",
            cat(&["-----", "BEGIN PGP ", "MESSAGE-----\n", &body(64)]),
        ),
        (
            "pem_private_key_marker",
            cat(&[&body(64), "\n-----END RSA ", "PRIVATE", " KEY-----"]),
        ),
        (
            "aws_secret_access_key",
            cat(&["aws_secret_", "access_key = ", &body(30), "/+", &body(8)]),
        ),
        (
            "aws_access_key_id",
            cat(&["{\"id\":\"", "AK", "IA", &upper_body(16), "\"}"]),
        ),
        (
            "github_fine_grained_pat",
            cat(&["token: ", "github_", "pat_", &body(22), "_", &body(59)]),
        ),
        ("github_token", cat(&["export T=", "gh", "p_", &body(36)])),
        (
            "slack_token",
            cat(&[
                "Authorization: Bearer ",
                "xo",
                "xb-",
                "1234567890-",
                &body(24),
            ]),
        ),
        (
            "sk_api_key",
            cat(&["{\"api_key\": \"", "s", "k-proj-", &body(48), "\"}"]),
        ),
    ]
}

#[test]
fn ordinary_json_with_sk_dash_substrings_passes() {
    let json = br#"{
      "task-id": "task-20261008-0001",
      "risk-level": "high", "risk-score": 0.82,
      "disk-usage": "73%", "ask-price": 12.5, "desk-ref": "desk-7",
      "steps": ["ask-user", "assess-risk-impact", "flush-disk-cache"],
      "note": "task-, risk-, disk-, ask- prefixes are ordinary words",
      "env": "export TOKEN=\"$TOKEN_FROM_ENV\"",
      "aws": "AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}"
    }"#;
    assert_eq!(matching_secret_rules(json), Vec::<&str>::new());
}

#[test]
fn every_real_format_is_rejected_by_exactly_its_rule() {
    let samples = real_samples();
    let covered: Vec<&str> = samples.iter().map(|(name, _)| *name).collect();
    let all: Vec<&str> = RULES.iter().map(|rule| rule.name).collect();
    assert_eq!(covered, all, "every rule needs a positive sample");
    for (name, sample) in samples {
        assert_eq!(
            matching_secret_rules(sample.as_bytes()),
            vec![name],
            "sample for {name} must match exactly that rule"
        );
    }
}

/// Mutation check: removing any single rule lets its sample through, so no
/// rule is redundant and the suite fails if a rule is weakened or dropped.
#[test]
fn removing_any_rule_lets_its_sample_through() {
    for (name, sample) in real_samples() {
        let still_blocked = RULES
            .iter()
            .filter(|rule| rule.name != name)
            .any(|rule| rule.pattern.is_match(&sample));
        assert!(!still_blocked, "{name} is shadowed by another rule");
    }
}

#[test]
fn real_formats_are_rejected_in_common_contexts() {
    let sk = cat(&["s", "k-", &body(48)]);
    let ant = cat(&["s", "k-ant-api03-", &body(80)]);
    let hex = cat(&["s", "k-", "0123456789abcdef0123456789abcdef"]);
    let gho = cat(&["gh", "o_", &body(36)]);
    let ghs = cat(&["gh", "s_", &body(36)]);
    let xoxp = cat(&["xo", "xp-", "1234-5678-", &body(12)]);
    let asia = cat(&["AS", "IA", &upper_body(16)]);
    let aws_json = cat(&["{\"SecretAccess", "Key\": \"", &body(40), "\"}"]);
    let aws_upper = cat(&["AWS_SECRET_", "ACCESS_KEY='", &body(40), "'"]);
    let pem_in_string = cat(&["key = \"\"\"\n-----", "BEGIN EC ", "PRIVATE KEY-----\nMHc"]);
    let cjk = cat(&["密钥", "s", "k-", &body(32)]);
    for sample in [
        sk.clone(),
        format!("Bearer {sk}"),
        format!("{{\"k\":\"{sk}\"}}"),
        format!("OPENAI_API_KEY={sk}"),
        ant,
        hex,
        gho,
        ghs,
        xoxp,
        asia,
        aws_json,
        aws_upper,
        pem_in_string,
        cjk,
    ] {
        assert!(
            contains_plaintext_secret(sample.as_bytes()),
            "must block: {sample}"
        );
    }
}

#[test]
fn near_miss_mutations_are_not_secrets() {
    let near_misses = [
        // Prefix not at a word boundary.
        cat(&["task", "-", &body(40)]),
        cat(&["ri", "sk-", &body(40)]),
        cat(&["my_", "s", "k-", &body(40)]),
        cat(&["x", "gh", "p_", &body(36)]),
        // One token character short.
        cat(&["s", "k-", &body(19)]),
        cat(&["gh", "p_", &body(35)]),
        cat(&["github_", "pat_", &body(21)]),
        cat(&["xo", "xb-", &body(9)]),
        cat(&["AK", "IA", &upper_body(15)]),
        cat(&["aws_secret_", "access_key=", &body(39)]),
        // Case differs from the real format.
        cat(&["S", "K-", &body(40)]),
        cat(&["G", "HP_", &body(36)]),
        cat(&["XO", "XB-", &body(20)]),
        cat(&["ak", "ia", &upper_body(16)]),
        cat(&["-----", "begin rsa private key-----"]),
        // Embedded in a longer identifier.
        cat(&["AK", "IA", &upper_body(17)]),
        // Value referenced, not embedded.
        cat(&["aws_secret_", "access_key: ${{ secrets.AWS_SECRET }}"]),
    ];
    for sample in near_misses {
        assert!(
            !contains_plaintext_secret(sample.as_bytes()),
            "must not block: {sample}"
        );
    }
}
