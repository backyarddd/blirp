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
    /// Extra check on the replaced text; false leaves the match alone.
    valid: fn(&str) -> bool,
}

const MARK: &str = "[REDACTED:";

fn rule(kind: &'static str, pattern: &str, group: usize, min_entropy: f64) -> Rule {
    checked(kind, pattern, group, min_entropy, |_| true)
}

fn checked(
    kind: &'static str,
    pattern: &str,
    group: usize,
    min_entropy: f64,
    valid: fn(&str) -> bool,
) -> Rule {
    Rule {
        kind,
        // Patterns are compile-time constants covered by the tests below; a bad
        // one fails every test run, so this cannot fail in a shipped build.
        #[allow(clippy::expect_used)]
        re: Regex::new(pattern).expect("redaction pattern must compile"),
        group,
        min_entropy,
        valid,
    }
}

/// Key names whose value is a secret whatever it looks like (human passwords
/// have low entropy). Matched as the whole key: `password`, `apiKey`, not
/// `db_password_hint` or `max_tokens`.
const SECRET_KEYS: &str = r"(?:password|passwd|passphrase|pwd|secret|client[_-]?secret|secret[_-]?key|api[_-]?key|access[_-]?key|auth[_-]?key|token|access[_-]?token|refresh[_-]?token|auth[_-]?token)";

/// False for docs/config placeholders: `${VAR}`, `<your-token>`, `{{ x }}`,
/// `xxxxxx`, `changeme`, `your_api_key_here`.
fn not_placeholder(v: &str) -> bool {
    let l = v.to_ascii_lowercase();
    let first = v.chars().next();
    let word = l.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    !(v.starts_with(MARK)
        || v.contains("${")
        || v.contains("{{")
        || v.contains("%(")
        || v.contains("...")
        || v.starts_with('$')
        || (v.starts_with('<') && v.ends_with('>'))
        || (v.starts_with('{') && v.ends_with('}'))
        || (v.starts_with('%') && v.ends_with('%'))
        || v.chars().all(|c| Some(c) == first)
        || l.starts_with("xxx")
        || l.contains("***")
        || l.starts_with("your")
        || [
            "changeme",
            "change_me",
            "change-me",
            "placeholder",
            "example",
            "dummy",
        ]
        .iter()
        .any(|w| l.contains(w))
        || [
            "password",
            "passwd",
            "secret",
            "token",
            "string",
            "required",
            "optional",
            "undefined",
            "hidden",
            "none",
            "null",
        ]
        .contains(&word))
}

fn is_ident(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Member path in code (`settings.API_KEY_V2`, `Self::TOKEN`), not a value.
/// Segments mixing lower, upper and digits look like token parts
/// (`MTk4NjIy.Cl2FMQ.ZnCjm1X`), so those stay secrets.
fn is_code_path(v: &str) -> bool {
    let random = |p: &str| {
        p.chars().any(|c| c.is_ascii_lowercase())
            && p.chars().any(|c| c.is_ascii_uppercase())
            && p.chars().any(|c| c.is_ascii_digit())
    };
    let path = v.replace("::", ".");
    path.contains('.') && path.split('.').all(|p| is_ident(p) && !random(p))
}

/// Unquoted value after a secret key: also rule out code (`String`,
/// `config.password`, `Self::TOKEN`, `get_token`) and filesystem paths
/// (`pwd: /home/me`). A bare identifier with a digit (`hunter22`) still counts.
fn bare_secret(v: &str) -> bool {
    let code = is_code_path(v) || (is_ident(v) && !v.chars().any(|c| c.is_ascii_digit()));
    let fs_path = v.starts_with(['/', '~', '.'])
        || v.contains("://")
        || v.get(1..3).is_some_and(|s| s == ":\\" || s == ":/");
    not_placeholder(v) && !code && !fs_path
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
        // Service Bus / Event Hubs / IoT Hub connection strings.
        rule(
            "azure_storage_key",
            r"(?i)SharedAccessKey=([A-Za-z0-9+/]{40,}=*)",
            1,
            0.0,
        ),
        // SAS token signature (`...&sig=<base64, often %-encoded>`).
        rule("azure_sas", r"(?i)[?&;]sig=([A-Za-z0-9%+/]{30,}=*)", 1, 0.0),
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
            r"\b(?:sk|rk)_(?:test|live|prod)_[0-9A-Za-z]{10,}\b",
            0,
            0.0,
        ),
        rule("stripe", r"\bwhsec_[0-9A-Za-z]{24,}\b", 0, 0.0),
        rule(
            "anthropic",
            r"\bsk-ant-[a-z]{2,6}\d{2}-[A-Za-z0-9_-]{32,}",
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
        // Bare `sk-<32+ alnum>`: legacy OpenAI and the many APIs copying its format.
        rule("api_key", r"\bsk-[A-Za-z0-9]{32,}\b", 0, 0.0),
        rule("npm", r"\bnpm_[A-Za-z0-9]{36}\b", 0, 0.0),
        rule("huggingface", r"\bhf_[A-Za-z0-9]{30,}\b", 0, 0.0),
        rule(
            "sendgrid",
            r"\bSG\.[A-Za-z0-9_-]{16,32}\.[A-Za-z0-9_-]{16,64}",
            0,
            0.0,
        ),
        rule("google_oauth", r"\bya29\.[0-9A-Za-z_-]{20,}", 0, 0.0),
        rule("shopify", r"\bshp(?:at|ss|ca|pa)_[a-fA-F0-9]{32}\b", 0, 0.0),
        rule("digitalocean", r"\bdo[opr]_v1_[a-f0-9]{64}\b", 0, 0.0),
        rule("pypi", r"\bpypi-AgE[A-Za-z0-9_-]{50,}", 0, 0.0),
        rule("slack", r"\bxapp-\d-[A-Za-z0-9-]{10,}", 0, 0.0),
        rule("twilio", r"\bSK[0-9a-fA-F]{32}\b", 0, 0.0),
        rule(
            "twilio",
            r#"(?i)twilio.{0,20}?(?:auth|token|secret).{0,20}?['"]?\s*[:=]\s*['"]?([a-f0-9]{32})\b"#,
            1,
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
        // Opaque credentials in an Authorization header (JWTs and provider tokens
        // are already named above).
        checked(
            "auth_header",
            r#"(?i)\bauthorization["']?[ \t]*[:=][ \t]*["']?(?:bearer|basic|token|bot)[ \t]+([A-Za-z0-9._~+/-]{8,}=*)"#,
            1,
            0.0,
            not_placeholder,
        ),
        // .env-style line whose variable name says it holds a secret. `R`: CRLF
        // text (Windows files, terminal output) ends lines with `\r\n`.
        rule(
            "env",
            r##"(?mR)^[ \t]*(?:export[ \t]+)?[A-Z][A-Z0-9_]*(?:KEY|SECRET|TOKEN|PASSWORD|PASSWD|PASS|PWD|CREDENTIALS?|DSN|DATABASE_URL|PRIVATE)[A-Z0-9_]*[ \t]*=[ \t]*['"]?([^\s'"#][^'"\r\n]*?)['"]?[ \t]*$"##,
            1,
            0.0,
        ),
        // key = value assignments in code/config with a high-entropy value.
        checked(
            "generic_secret",
            r#"(?i)\b[a-z0-9_.-]*(?:api[_-]?key|secret|token|passw(?:or)?d|pwd|auth[_-]?key|credential|access[_-]?key)[a-z0-9_.-]*['"]?\s*(?::=|=>|[:=])\s*['"]?([A-Za-z0-9_\-+/=.~!@#$%^&*]{8,})"#,
            1,
            3.5,
            |v| !is_code_path(v),
        ),
        // Exact secret key names: redact the low-entropy values (human
        // passwords) the generic rule lets through.
        checked(
            "secret",
            &format!(
                r#"(?i)(?:^|[^a-z0-9_]){SECRET_KEYS}["'`]?[ \t]*(?::=|=>|[:=])[ \t]*["'`]([^"'`\r\n]{{6,}})["'`]"#
            ),
            1,
            0.0,
            not_placeholder,
        ),
        checked(
            "secret",
            &format!(
                r#"(?i)(?:^|[^a-z0-9_]){SECRET_KEYS}["'`]?[ \t]*(?::=|=>|[:=])[ \t]*([^\s"'`,;(){{}}\[\]<>=&*$%!|\\][^\s"'`,;(){{}}\[\]<>&|]{{5,}})"#
            ),
            1,
            0.0,
            bare_secret,
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
            if value.starts_with(MARK)
                || (r.min_entropy > 0.0 && entropy(value) < r.min_entropy)
                || !(r.valid)(value)
            {
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
        // Member paths are code; dotted random tokens are not.
        assert_clean("apiKey = settings.OPENAI_API_KEY_V2");
        assert_redacted(
            concat!("bot_token = MTk4NjIyNDgzNDcxOTI1MjQ4", ".Cl2FMQ.ZnCjm1XVW7vRze4b7Cq4se7kKWs"),
            "generic_secret",
        );
    }

    #[test]
    fn crlf_text() {
        assert_eq!(
            redact("GH_TOKEN=abc\r\nDEBUG=true\r\n"),
            "GH_TOKEN=[REDACTED:env]\r\nDEBUG=true\r\n"
        );
        assert_eq!(
            redact("export API_KEY=\"hunter2\"\r\nDB_PASSWORD=correct horse\r"),
            "export API_KEY=\"[REDACTED:env]\"\r\nDB_PASSWORD=[REDACTED:env]\r"
        );
        assert_clean("DEBUG=true\r\nPORT=8080\r\nAPI_KEY=\r\n");
        assert_eq!(
            redact(
                "-----BEGIN RSA PRIVATE KEY-----\r\nMIIEpAIBAAKCAQEA1234567890abcdef\r\n-----END RSA PRIVATE KEY-----\r\nok"
            ),
            "[REDACTED:private_key]\r\nok"
        );
        assert_redacted(
            "-----BEGIN PRIVATE KEY-----\r\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASC\r\nMIIEvQIBADANBgkq",
            "private_key",
        );
        assert_eq!(
            redact("password: hunter22\r\nuser: bob\r\n"),
            "password: [REDACTED:secret]\r\nuser: bob\r\n"
        );
        assert_eq!(
            redact("Authorization: Bearer abcdef1234567890\r\nHost: x\r\n"),
            "Authorization: Bearer [REDACTED:auth_header]\r\nHost: x\r\n"
        );
    }

    #[test]
    fn exact_secret_keys() {
        for (input, want) in [
            (
                "password = \"hunter22\"",
                "password = \"[REDACTED:secret]\"",
            ),
            (
                "{\"password\": \"correct horse\"}",
                "{\"password\": \"[REDACTED:secret]\"}",
            ),
            ("pwd: 'letmein'", "pwd: '[REDACTED:secret]'"),
            ("passwd=hunter22", "passwd=[REDACTED:secret]"),
            (
                "mysql --password=Summer2024! db",
                "mysql --password=[REDACTED:secret] db",
            ),
            ("secret: hunter22", "secret: [REDACTED:secret]"),
            (
                "self.password = `qwerty`",
                "self.password = `[REDACTED:secret]`",
            ),
            ("apiKey: 'key-1234'", "apiKey: '[REDACTED:secret]'"),
            ("TOKEN := \"abcdef\"", "TOKEN := \"[REDACTED:secret]\""),
            (
                "client_secret => 'topsecret'",
                "client_secret => '[REDACTED:secret]'",
            ),
            ("url?token=abc123def", "url?token=[REDACTED:secret]"),
        ] {
            assert_eq!(redact(input), want, "{input}");
        }
        for clean in [
            "password: string",
            "pub token: Option<String>,",
            "token: CancellationToken,",
            "let token: String = String::new();",
            "password = form.password",
            "token = get_token()",
            "api_key = os.environ[\"API_KEY\"]",
            "api_key = settings.API_KEY_V2",
            "password: \"${DB_PASSWORD}\"",
            "token: ${{ secrets.GITHUB_TOKEN }}",
            "password: \"<your-password>\"",
            "password: \"{{ vault_pw }}\"",
            "password: \"changeme\"",
            "password = \"xxxxxxxx\"",
            "password: ********",
            "token: \"your_token_here\"",
            "api_key: \"...\"",
            "password = \"\"",
            "password: short",
            "password: !vault |",
            "pwd: /home/user/project",
            "PWD=C:\\Users\\me",
            "the token is invalid; refresh the token and retry",
            "Token: expired",
            "\"max_tokens\": 4096, \"tokens\": 123456",
            "db_password_hint = \"hunter22\"",
        ] {
            assert_clean(clean);
        }
    }

    #[test]
    fn authorization_header() {
        assert_redacted(
            "curl -H \"Authorization: Bearer abcdef1234567890opaque\" https://x",
            "auth_header",
        );
        assert_eq!(
            redact("Authorization: Basic dXNlcjpwYXNzd29yZA=="),
            "Authorization: Basic [REDACTED:auth_header]"
        );
        assert_redacted(
            "{\"Authorization\": \"Token 0123456789abcdef\"}",
            "auth_header",
        );
        assert_redacted(
            "Proxy-Authorization: bearer zzTopSecretValue",
            "auth_header",
        );
        for clean in [
            "Authorization: Bearer <token>",
            "Authorization: Bearer $TOKEN",
            "Authorization: Bearer ${TOKEN}",
            "Authorization: Bearer YOUR_TOKEN",
            "Authorization: Bearer xxxxxxxxxx",
            "authorization: required for all endpoints",
            "Bearer tokens go in the Authorization header",
        ] {
            assert_clean(clean);
        }
    }

    #[test]
    fn provider_prefixes() {
        let hex32 = "0123456789abcdef".repeat(2);
        for (input, kind) in [
            (format!("npm_{}", "a1B2".repeat(9)), "npm"),
            (format!("hf_{}", "AbCd".repeat(9)), "huggingface"),
            (
                format!(
                    "SG.{}.{}",
                    "aB3_".repeat(5) + "xy",
                    "Qw9-".repeat(10) + "abc"
                ),
                "sendgrid",
            ),
            (format!("ya29.{}", "a0AfH6".repeat(8)), "google_oauth"),
            (format!("shpat_{hex32}"), "shopify"),
            (format!("shpss_{hex32}"), "shopify"),
            (format!("shpca_{hex32}"), "shopify"),
            (format!("shppa_{hex32}"), "shopify"),
            (format!("dop_v1_{}", hex32.repeat(2)), "digitalocean"),
            (format!("doo_v1_{}", hex32.repeat(2)), "digitalocean"),
            (format!("pypi-AgEIcHlwaS5vcmc{}", "Ab1-".repeat(15)), "pypi"),
            (
                "xapp-1-A0123456789-1234567890123-abcdef0123456789".to_string(),
                "slack",
            ),
            (format!("sk-ant-api03-{}AA", "Ab1_".repeat(23)), "anthropic"),
            (format!("sk-ant-oat01-{}", "Ab1-".repeat(12)), "anthropic"),
            (format!("sk-{}", "Ab12".repeat(12)), "api_key"),
            (
                format!("github_pat_11ABCDEFG0123456789abc_{}", "aB3".repeat(20)),
                "github",
            ),
            (format!("SK{hex32}"), "twilio"),
            (format!("twilio_auth_token = \"{hex32}\""), "twilio"),
            (
                format!(
                    "Endpoint=sb://x.servicebus.windows.net/;SharedAccessKeyName=Root;SharedAccessKey={}=",
                    "Ab1+".repeat(11)
                ),
                "azure_storage_key",
            ),
            (
                format!(
                    "https://a.blob.core.windows.net/c?sv=2022-11-02&sig={}%3D",
                    "Ab1%2B".repeat(6)
                ),
                "azure_sas",
            ),
        ] {
            assert_redacted(&input, kind);
        }
        // Newer Stripe keys run past 99 chars; the whole key must go.
        let stripe = format!("sk_live_{}", "a1B2c3".repeat(18));
        assert_eq!(redact(&stripe), "[REDACTED:stripe]");
        assert_redacted(&format!("rk_live_{}", "a1B2c3".repeat(4)), "stripe");

        for clean in [
            "npm_config_cache and npm_package_version",
            "hf_hub_download(hf_token)",
            "SG.Alert and SG.x.y",
            "ya29.short",
            "shpat_notahexvalue",
            "dop_v1_abc",
            "pypi-server and pypi-simple",
            "xapp-foo",
            "sk-ant-api03-short",
            "sk-learn task-12345 risk-assessment",
            "SKU12345 and SK0123456789abcdef",
            "?sig=short",
        ] {
            assert_clean(clean);
        }
        // The key name is not the key (the generic rule may still judge the value).
        assert!(!redact("SharedAccessKeyName=RootManageSharedAccessKey").contains("azure"));
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
