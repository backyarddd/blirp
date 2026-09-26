//! Secret redaction (§9). Applied to every transcript text and meta value before
//! it is stored, synced or summarized. Rules follow gitleaks' rule set; matches
//! are replaced with `[REDACTED:<kind>]`.

use regex::{Captures, Regex};
use serde_json::Value;
use std::borrow::Cow;
use std::sync::LazyLock;

struct Rule {
    kind: &'static str,
    re: Regex,
    /// Capture group to replace; 0 = whole match.
    group: usize,
    /// Minimum Shannon entropy (bits/char) of the replaced text, 0 = none.
    min_entropy: f64,
}

const MARK: &str = "[REDACTED:";

fn rule(kind: &'static str, pattern: &str, group: usize, min_entropy: f64) -> Rule {
    Rule {
        kind,
        // Patterns are compile-time constants covered by the tests below; a bad
        // one fails every test run, so this cannot fail in a shipped build.
        #[allow(clippy::expect_used)]
        re: Regex::new(pattern).expect("redaction pattern must compile"),
        group,
        min_entropy,
    }
}

/// Order matters: specific provider rules run before the generic ones so the
/// replacement names the provider.
static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        rule(
            "private_key",
            r"-----BEGIN[ A-Z0-9_-]{0,100}PRIVATE KEY(?: BLOCK)?-----[\s\S]*?-----END[ A-Z0-9_-]{0,100}PRIVATE KEY(?: BLOCK)?-----",
            0,
            0.0,
        ),
        // A key block cut off by truncation still leaks its body.
        rule(
            "private_key",
            r"-----BEGIN[ A-Z0-9_-]{0,100}PRIVATE KEY(?: BLOCK)?-----(?:\s*[A-Za-z0-9+/=]{16,})+",
            0,
            0.0,
        ),
        rule(
            "aws_access_key",
            r"\b(?:A3T[A-Z0-9]|AKIA|ASIA|ABIA|ACCA)[A-Z2-7]{16}\b",
            0,
            0.0,
        ),
        rule(
            "aws_secret_key",
            r#"(?i)aws.{0,20}?(?:secret|private|key).{0,20}?['"]?\s*[:=]\s*['"]?([A-Za-z0-9/+=]{40})\b"#,
            1,
            3.0,
        ),
        rule("gcp_api_key", r"\bAIza[0-9A-Za-z_-]{35}\b", 0, 0.0),
        rule(
            "azure_storage_key",
            r"(?i)AccountKey=([A-Za-z0-9+/=]{80,100})",
            1,
            0.0,
        ),
        rule(
            "azure_client_secret",
            r"\b[a-zA-Z0-9_~.]{3}\dQ~[a-zA-Z0-9_~.-]{31,34}\b",
            0,
            0.0,
        ),
        rule(
            "github",
            r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36,255}\b",
            0,
            0.0,
        ),
        rule("github", r"\bgithub_pat_[A-Za-z0-9_]{50,255}\b", 0, 0.0),
        rule(
            "gitlab",
            r"\bgl(?:pat|dt|rt|ptt|cbt|soat|ft)-[0-9A-Za-z_-]{20,}\b",
            0,
            0.0,
        ),
        rule("slack", r"\bxox[abposr]-[0-9A-Za-z-]{10,}\b", 0, 0.0),
        rule(
            "slack_webhook",
            r"https://hooks\.slack\.com/(?:services|workflows|triggers)/[A-Za-z0-9+/_-]{20,}",
            0,
            0.0,
        ),
        rule(
            "stripe",
            r"\b(?:sk|rk)_(?:test|live|prod)_[0-9A-Za-z]{10,99}\b",
            0,
            0.0,
        ),
        rule("stripe", r"\bwhsec_[0-9A-Za-z]{24,}\b", 0, 0.0),
        rule(
            "anthropic",
            r"\bsk-ant-(?:api|admin|oat)\d{2}-[A-Za-z0-9_-]{80,}",
            0,
            0.0,
        ),
        rule(
            "openai",
            r"\bsk-(?:(?:proj|svcacct|admin)-)?[A-Za-z0-9_-]{20,}T3BlbkFJ[A-Za-z0-9_-]{20,}",
            0,
            0.0,
        ),
        rule(
            "openai",
            r"\bsk-(?:proj|svcacct|admin)-[A-Za-z0-9_-]{40,}",
            0,
            0.0,
        ),
        rule(
            "jwt",
            r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
            0,
            0.0,
        ),
        // scheme://user:password@host - only the password is replaced.
        rule(
            "connection_string",
            r#"\b[a-zA-Z][a-zA-Z0-9+.-]{1,20}://[^\s:/@'"]+:([^\s@/'"]+)@"#,
            1,
            0.0,
        ),
        // .env-style line whose variable name says it holds a secret.
        rule(
            "env",
            r##"(?m)^[ \t]*(?:export[ \t]+)?[A-Z][A-Z0-9_]*(?:KEY|SECRET|TOKEN|PASSWORD|PASSWD|PASS|PWD|CREDENTIALS?|DSN|DATABASE_URL|PRIVATE)[A-Z0-9_]*[ \t]*=[ \t]*['"]?([^\s'"#][^'"\r\n]*?)['"]?[ \t]*$"##,
            1,
            0.0,
        ),
        // key = value assignments in code/config with a high-entropy value.
        rule(
            "generic_secret",
            r#"(?i)\b[a-z0-9_.-]*(?:api[_-]?key|secret|token|passw(?:or)?d|pwd|auth[_-]?key|credential|access[_-]?key)[a-z0-9_.-]*['"]?\s*(?::=|=>|[:=])\s*['"]?([A-Za-z0-9_\-+/=.~!@#$%^&*]{8,})"#,
            1,
            3.5,
        ),
    ]
});

/// Shannon entropy in bits per character.
fn entropy(s: &str) -> f64 {
    let mut counts = std::collections::HashMap::new();
    let mut n = 0usize;
    for c in s.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
        n += 1;
    }
    if n == 0 {
        return 0.0;
    }
    let n = n as f64;
    counts
        .values()
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// Redact secrets in `text`. Borrows when nothing matched.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(text);
    for r in RULES.iter() {
        if !r.re.is_match(&out) {
            continue;
        }
        let replaced = r.re.replace_all(&out, |caps: &Captures<'_>| {
            let whole = caps.get(0).map_or("", |m| m.as_str());
            let Some(target) = caps.get(r.group) else {
                return whole.to_string();
            };
            let value = target.as_str();
            if value.starts_with(MARK) || (r.min_entropy > 0.0 && entropy(value) < r.min_entropy) {
                return whole.to_string();
            }
            let base = caps.get(0).map_or(0, |m| m.start());
            let (s, e) = (target.start() - base, target.end() - base);
            format!("{}{MARK}{}]{}", &whole[..s], r.kind, &whole[e..])
        });
        if let Cow::Owned(s) = replaced {
            out = Cow::Owned(s);
        }
    }
    out
}

/// Redact every string (keys untouched) inside a JSON value in place.
pub fn redact_json(value: &mut Value) {
    match value {
        Value::String(s) => {
            if let Cow::Owned(r) = redact(s) {
                *s = r;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_json),
        Value::Object(map) => map.values_mut().for_each(redact_json),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_redacted(input: &str, kind: &str) {
        let out = redact(input);
        assert!(
            out.contains(&format!("[REDACTED:{kind}]")),
            "expected {kind} in {out:?} (input {input:?})"
        );
    }

    fn assert_clean(input: &str) {
        assert_eq!(redact(input), input, "unexpected redaction");
    }

    #[test]
    fn private_key() {
        let key = "-----BEGIN RSA PRIVATE KEY-----\nMIIEpAIBAAKCAQEA1234567890abcdef\nabcdefABCDEF0123456789+/==\n-----END RSA PRIVATE KEY-----";
        let input = format!("key:\n{key}\ndone");
        let out = redact(&input);
        assert_eq!(out, "key:\n[REDACTED:private_key]\ndone");
        assert_redacted(
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ",
            "private_key",
        );
        assert_clean(
            "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8A\n-----END PUBLIC KEY-----",
        );
    }

    #[test]
    fn aws() {
        assert_redacted("id=AKIAIOSFODNN7EXAMPLE", "aws_access_key");
        assert_clean("AKIA is a prefix, AKIAshort is not a key");
        assert_redacted(
            "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "aws_secret_key",
        );
        assert_clean("aws_region = us-east-1");
    }

    #[test]
    fn gcp() {
        assert_redacted("key=AIzaSyA1234567890abcdefghijklmnopqrstuv", "gcp_api_key");
        assert_clean("AIzaShort");
    }

    #[test]
    fn azure() {
        let key = "a".repeat(40) + &"B".repeat(40) + "0123==";
        assert_redacted(
            &format!("DefaultEndpointsProtocol=https;AccountName=x;AccountKey={key};"),
            "azure_storage_key",
        );
        assert_clean("AccountKey=short");
        assert_redacted(
            "client_secret: abc8Q~abcdefghijklmnopqrstuvwxyz0123456",
            "azure_client_secret",
        );
        assert_clean("version 8Q~ release");
    }

    #[test]
    fn github() {
        assert_redacted("ghp_abcdefghijklmnopqrstuvwxyzABCDEFGHIJ", "github");
        assert_redacted(&format!("github_pat_{}", "A1_".repeat(25)), "github");
        assert_clean("ghp_short and gh_pages");
    }

    #[test]
    fn gitlab() {
        assert_redacted("glpat-abcdefghij0123456789", "gitlab");
        assert_clean("glpat-short");
    }

    #[test]
    fn slack() {
        assert_redacted("xoxb-123456789012-abcdefghij", "slack");
        assert_redacted(
            "https://hooks.slack.com/services/T000/B000/XXXXXXXXXXXXXXXXXXXXXXXX",
            "slack_webhook",
        );
        assert_clean("xoxo hugs and https://hooks.slack.com/");
    }

    #[test]
    fn stripe() {
        assert_redacted(concat!("sk_", "live_4eC39HqLyjWDarjtT1zdp7dc"), "stripe");
        assert_redacted(concat!("rk_", "test_4eC39HqLyjWDarjtT1zdp7dc"), "stripe");
        assert_redacted("whsec_abcdefghijklmnopqrstuvwxyz012345", "stripe");
        assert_clean(concat!("pk_", "live_4eC39HqLyjWDarjtT1zdp7dc is publishable"));
    }

    #[test]
    fn anthropic_and_openai() {
        assert_redacted(&format!("sk-ant-api03-{}", "x".repeat(90)), "anthropic");
        assert_redacted(
            &format!("sk-proj-{}T3BlbkFJ{}", "a".repeat(24), "b".repeat(24)),
            "openai",
        );
        assert_redacted(&format!("sk-proj-{}", "Ab1_".repeat(12)), "openai");
        assert_clean("sk-learn and sk-ant are words; task-proj-1");
    }

    #[test]
    fn jwt() {
        assert_redacted(
            "Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
            "jwt",
        );
        assert_clean("eyJ alone is not a token");
    }

    #[test]
    fn connection_string() {
        let out = redact("DATABASE=postgres://app:s3cretPass@db.internal:5432/app");
        assert_eq!(
            out,
            "DATABASE=postgres://app:[REDACTED:connection_string]@db.internal:5432/app"
        );
        assert_clean("see https://github.com/owner/repo and git@github.com:o/r.git");
    }

    #[test]
    fn env_lines() {
        assert_redacted("export API_KEY=hunter2", "env");
        assert_redacted("DB_PASSWORD=\"correct horse\"", "env");
        assert_eq!(
            redact("GH_TOKEN=abc\nDEBUG=true"),
            "GH_TOKEN=[REDACTED:env]\nDEBUG=true"
        );
        assert_clean("DEBUG=true\nPORT=8080\nAPI_KEY=");
    }

    #[test]
    fn generic_assignment() {
        assert_redacted("client_secret: \"Zx8#kP2qLm9vRt4w\"", "generic_secret");
        assert_redacted("let apiKey = 'q8F2kLx9Pz3mW7vB1nT6';", "generic_secret");
        assert_clean("max_tokens = 4096");
        assert_clean("password: string");
        assert_clean("token = aaaaaaaaaaaa");
    }

    #[test]
    fn already_redacted_is_stable() {
        let once =
            redact("OPENAI_API_KEY=sk-proj-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
                .into_owned();
        assert_eq!(redact(&once), once);
        assert!(once.contains("[REDACTED:"));
    }

    #[test]
    fn json_values() {
        let mut v = serde_json::json!({"cmd": "curl -H 'Authorization: Bearer ghp_abcdefghijklmnopqrstuvwxyzABCDEFGHIJ'", "n": 1, "list": ["AKIAIOSFODNN7EXAMPLE"]});
        redact_json(&mut v);
        let s = v.to_string();
        assert!(!s.contains("ghp_abc"), "{s}");
        assert!(!s.contains("AKIAIOSFODNN7EXAMPLE"), "{s}");
        assert_eq!(v["n"], 1);
    }
}
