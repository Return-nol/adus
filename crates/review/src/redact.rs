//! Masks values that look like secrets (private keys, provider tokens, credentials in URLs and
//! named assignments) before repository text reaches the model and before a comment is posted.
//!
//! Redaction replaces text within a line and never adds or removes a line, so the numbered
//! output of `read_file` and `grep` keeps its line numbers. The patterns are written from the
//! public token formats and run on the linear-time `regex` engine with a size limit.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

use regex::{Captures, Match, Regex};

use crate::prompt::truncate_with_marker;

/// What a masked value is replaced with.
pub(crate) const REDACTED: &str = "[REDACTED]";

/// Most characters of a model's rationale kept for a log line.
pub(crate) const RATIONALE_LOG_CHARS: usize = 300;

/// Shortest value of a named assignment that is masked.
const MIN_VALUE_CHARS: usize = 8;

/// Key names whose assigned value is masked (case-insensitive, anywhere in the name).
const SECRET_KEY_NAME: &str = r"[A-Za-z0-9_.-]*(?:password|passwd|secret|api_key|apikey|api-key|access_token|auth_token|client_secret|private_key)[A-Za-z0-9_.-]*";

/// Provider token shapes; the whole match is masked.
const TOKENS: &str = concat!(
    r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
    r"|\bgh[pousr]_[A-Za-z0-9]{36,}\b",
    r"|\bgithub_pat_[A-Za-z0-9_]{22,}\b",
    r"|\bsk-[A-Za-z0-9_-]{20,}",
    r"|\bAIza[0-9A-Za-z_-]{35}\b",
    r"|\bxox[abposr]-[A-Za-z0-9-]{10,}",
    r"|\b[rs]k_(?:live|test)_[A-Za-z0-9]{16,}\b",
    r"|\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
);

/// `scheme://user:password@`; the password is masked.
const URL_CREDENTIALS: &str = r"(?i)\b[a-z][a-z0-9+.-]*://[^\s:/@]+:(?P<value>[^\s@/]+)@";

const PEM_BEGIN: &str = r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----";
const PEM_END: &str = r"-----END (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----";

struct Patterns {
    pem_begin: Regex,
    pem_end: Regex,
    tokens: Regex,
    url_credentials: Regex,
    /// A secret key name, `=`/`:` (or `:=`, `=>`, `==`) and a quoted literal.
    quoted_assignment: Regex,
    /// An env-style line `KEY=value` at the start of the line (optional `export `, optional
    /// tool line-number prefix; no indentation, so keyword arguments and props never match).
    env_assignment: Regex,
}

impl Patterns {
    fn compile() -> Option<Self> {
        let quoted = format!(
            r#"(?i){SECRET_KEY_NAME}["']?[ \t]*(?::=|=>|==|[:=])[ \t]*(?:"(?P<dq>[^"\r\n]*)"|'(?P<sq>[^'\r\n]*)')"#
        );
        let env = format!(
            r"(?mi)^(?:\d+[:>-] )?(?:export[ \t]+)?{SECRET_KEY_NAME}=(?P<value>[^\s'\x22]\S*)[ \t\r]*$"
        );
        let patterns = Self {
            pem_begin: build(PEM_BEGIN)?,
            pem_end: build(PEM_END)?,
            tokens: build(TOKENS)?,
            url_credentials: build(URL_CREDENTIALS)?,
            quoted_assignment: build(&quoted)?,
            env_assignment: build(&env)?,
        };
        Some(patterns)
    }
}

fn build(pattern: &str) -> Option<Regex> {
    match Regex::new(pattern) {
        Ok(regex) => Some(regex),
        Err(err) => {
            tracing::error!(error = %err, "a secret redaction pattern does not compile");
            None
        }
    }
}

static PATTERNS: LazyLock<Option<Patterns>> = LazyLock::new(Patterns::compile);

/// A model's rationale for a log line: secret-redacted, then cut to [`RATIONALE_LOG_CHARS`]
/// characters plus an ellipsis.
pub(crate) fn log_rationale(text: &str) -> String {
    truncate_with_marker(&redact_secrets(text), RATIONALE_LOG_CHARS, "…").into_owned()
}

/// `text` with every value that looks like a secret replaced by [`REDACTED`];
/// `Cow::Borrowed` when nothing matched.
pub(crate) fn redact_secrets(text: &str) -> Cow<'_, str> {
    let Some(p) = PATTERNS.as_ref() else {
        return Cow::Borrowed(text);
    };
    let mut out = Cow::Borrowed(text);
    let edits = pem_edits(p, &out);
    apply(&mut out, &edits);
    let edits: Vec<_> = p.tokens.find_iter(&out).map(|m| m.range()).collect();
    apply(&mut out, &edits);
    let edits = value_edits(&p.url_credentials, &out, 1, |c| c.name("value"));
    apply(&mut out, &edits);
    let edits = value_edits(&p.quoted_assignment, &out, MIN_VALUE_CHARS, |c| {
        c.name("dq").or_else(|| c.name("sq"))
    });
    apply(&mut out, &edits);
    let edits = value_edits(&p.env_assignment, &out, MIN_VALUE_CHARS, |c| {
        c.name("value").filter(|v| !is_code_value(v.as_str()))
    });
    apply(&mut out, &edits);
    out
}

/// Replaces `edits` in `text` with [`REDACTED`]; leaves `text` (and its borrow) alone when
/// there are none.
fn apply(text: &mut Cow<'_, str>, edits: &[Range<usize>]) {
    if edits.is_empty() {
        return;
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for range in edits {
        out.push_str(&text[last..range.start]);
        out.push_str(REDACTED);
        last = range.end;
    }
    out.push_str(&text[last..]);
    *text = Cow::Owned(out);
}

/// Whether [`redact_secrets`] would change `text`.
pub(crate) fn contains_secret(text: &str) -> bool {
    matches!(redact_secrets(text), Cow::Owned(_))
}

/// The ranges of `regex`'s value (picked by `value`) that hold a real-looking secret: at least
/// `min_chars` characters and not a placeholder.
fn value_edits<'t>(
    regex: &Regex,
    text: &'t str,
    min_chars: usize,
    value: impl Fn(&Captures<'t>) -> Option<Match<'t>>,
) -> Vec<Range<usize>> {
    regex
        .captures_iter(text)
        .filter_map(|caps| value(&caps))
        .filter(|m| m.as_str().chars().count() >= min_chars && !is_placeholder(m.as_str()))
        .map(|m| m.range())
        .collect()
}

/// An unquoted value that is code rather than a literal: a call (`f(x)`), a JSX expression
/// (`{x}`) or an argument-list item (`x,`, `x)`).
fn is_code_value(value: &str) -> bool {
    value.contains('(') || value.starts_with('{') || value.ends_with([',', ')'])
}

/// Template variables, environment references and already-masked values.
fn is_placeholder(value: &str) -> bool {
    let v = value.trim();
    v.is_empty()
        || v.contains(REDACTED)
        || v.starts_with('$')
        || v.starts_with('%')
        || (v.starts_with('<') && v.ends_with('>'))
        || v.contains("${")
        || v.contains("{{")
}

/// Length of a `read_file` (`12: `) or `grep` (`12> `, `12- `) line-number prefix, or 0.
fn tool_prefix_len(line: &str) -> usize {
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    let rest = &line.as_bytes()[digits..];
    if digits > 0 && rest.len() >= 2 && matches!(rest[0], b':' | b'>' | b'-') && rest[1] == b' ' {
        digits + 2
    } else {
        0
    }
}

/// One line of the text: the byte range of its content after any tool prefix, without the
/// line break.
struct Line {
    body: Range<usize>,
    prefixed: bool,
}

/// The key lines after a private-key BEGIN line, and the text between both markers on one line.
/// After BEGIN only lines that look like a PEM body are masked (base64, `Proc-Type:` /
/// `DEK-Info:` headers; blank lines are passed over); the block ends at END, at the first other
/// line, or, when BEGIN carried a tool line-number prefix, at the first unprefixed line. So a
/// key cut by a tool window is still masked, while a BEGIN marker quoted in prose or code leaves
/// the following lines alone. Base64 lines right before an END with no BEGIN are masked too.
fn pem_edits(p: &Patterns, text: &str) -> Vec<Range<usize>> {
    if !text.contains("PRIVATE KEY") {
        return Vec::new();
    }
    let mut edits = Vec::new();
    let mut lines: Vec<Line> = Vec::new();
    let mut in_key = false;
    let mut begin_prefixed = false;
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let content = raw.trim_end_matches(['\n', '\r']);
        let prefix = tool_prefix_len(content);
        let start = offset + prefix;
        let body = &content[prefix..];
        offset += raw.len();
        if in_key && begin_prefixed && prefix == 0 {
            in_key = false;
        }
        let begin = p.pem_begin.find(body);
        let end = p.pem_end.find(body);
        match (begin, end) {
            (Some(b), Some(e)) if b.end() <= e.start() => {
                let inner = &body[b.end()..e.start()];
                if !inner.is_empty() && inner != REDACTED {
                    edits.push(start + b.end()..start + e.start());
                }
                in_key = false;
            }
            (Some(_), _) => {
                in_key = true;
                begin_prefixed = prefix > 0;
            }
            (None, Some(_)) => {
                if !in_key {
                    let before = lines
                        .iter()
                        .rev()
                        .take_while(|l| {
                            l.prefixed == (prefix > 0) && is_base64_line(&text[l.body.clone()])
                        })
                        .map(|l| l.body.clone());
                    edits.extend(before);
                }
                in_key = false;
            }
            (None, None) if in_key => {
                if is_pem_body_line(body) {
                    edits.push(start..start + body.len());
                } else if !body.trim().is_empty() {
                    in_key = false;
                }
            }
            (None, None) => {}
        }
        lines.push(Line {
            body: start..start + body.len(),
            prefixed: prefix > 0,
        });
    }
    edits.sort_by_key(|r| r.start);
    edits
}

/// A base64 line or a legacy PEM header (`Proc-Type: 4,ENCRYPTED`, `DEK-Info: …`).
fn is_pem_body_line(body: &str) -> bool {
    let body = body.trim_end();
    is_base64_line(body) || body.starts_with("Proc-Type:") || body.starts_with("DEK-Info:")
}

fn is_base64_line(body: &str) -> bool {
    !body.is_empty()
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Obviously fake values in the provider formats.
    const AWS: &str = "AKIAIOSFODNN7EXAMPLE";
    const GITHUB: &str = "ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn redacted(text: &str) -> String {
        redact_secrets(text).into_owned()
    }

    fn assert_untouched(text: &str) {
        assert!(
            matches!(redact_secrets(text), Cow::Borrowed(_)),
            "should not be redacted: {text:?} -> {:?}",
            redact_secrets(text)
        );
        assert!(!contains_secret(text), "{text:?}");
    }

    #[test]
    fn patterns_compile() {
        assert!(PATTERNS.is_some());
    }

    #[test]
    fn pem_private_keys_are_masked_preserving_line_counts() {
        let block = "const KEY: &str = r\"\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAfakefakefake\nZmFrZWZha2VmYWtlZmFrZQ==\n-----END RSA PRIVATE KEY-----\n\";\n";
        let block_out = redacted(block);
        assert_eq!(block_out.lines().count(), block.lines().count());
        assert!(contains_secret(block));

        let cases = [
            (
                block.to_string(),
                "const KEY: &str = r\"\n-----BEGIN RSA PRIVATE KEY-----\n[REDACTED]\n[REDACTED]\n-----END RSA PRIVATE KEY-----\n\";\n",
            ),
            (
                "[k.pem: lines 1-4 of 4]\n1: -----BEGIN PRIVATE KEY-----\n2: MIIEvfakefake\n3: Zm9vYmFy\n4: -----END PRIVATE KEY-----".into(),
                "[k.pem: lines 1-4 of 4]\n1: -----BEGIN PRIVATE KEY-----\n2: [REDACTED]\n3: [REDACTED]\n4: -----END PRIVATE KEY-----",
            ),
            (
                "5: Zm9vYmFyZm9vYmFy\n6: YmF6\n7: -----END OPENSSH PRIVATE KEY-----\n8: }".into(),
                "5: [REDACTED]\n6: [REDACTED]\n7: -----END OPENSSH PRIVATE KEY-----\n8: }",
            ),
            (
                "k.pem\n1> -----BEGIN EC PRIVATE KEY-----\n2- Zm9vYmFy\n3- YmF6YmF6\n\nsrc/a.rs\n4> let a = 1;".into(),
                "k.pem\n1> -----BEGIN EC PRIVATE KEY-----\n2- [REDACTED]\n3- [REDACTED]\n\nsrc/a.rs\n4> let a = 1;",
            ),
            (
                r#"{"key": "-----BEGIN PRIVATE KEY-----\nMIIfake\n-----END PRIVATE KEY-----\n"}"#.into(),
                r#"{"key": "-----BEGIN PRIVATE KEY-----[REDACTED]-----END PRIVATE KEY-----\n"}"#,
            ),
            (
                "[k.pem: lines 1-3 of 30]\n1: -----BEGIN PRIVATE KEY-----\n2: MIIEvQIBADANBgkqhkiG9w0B\n3: Zm9vYmFyZm9vYmFy".into(),
                "[k.pem: lines 1-3 of 30]\n1: -----BEGIN PRIVATE KEY-----\n2: [REDACTED]\n3: [REDACTED]",
            ),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,0123456789ABCDEF\n\nZm9vYmFyZm9vYmFy\n-----END RSA PRIVATE KEY-----".into(),
                "-----BEGIN RSA PRIVATE KEY-----\n[REDACTED]\n[REDACTED]\n\n[REDACTED]\n-----END RSA PRIVATE KEY-----",
            ),
        ];
        for (input, want) in cases {
            assert_eq!(redacted(&input), want, "{input:?}");
        }
    }

    #[test]
    fn pem_markers_in_prose_code_and_public_certs_are_untouched() {
        for text in [
            "<!-- adudu -->\n**Critical:** Private key committed\n\nThe file holds a `-----BEGIN RSA PRIVATE KEY-----` block. Rotate it.\n\nMove it to a secret store.\n\n```suggestion\nlet k = load_key()?;\n```",
            "if line.starts_with(\"-----BEGIN PRIVATE KEY-----\") {\n    parse(line);\n}\nfn other() { x() }",
            "[src/pem.rs: lines 4-7 of 9]\n4: if line.starts_with(\"-----BEGIN PRIVATE KEY-----\") {\n5:     parse(line);\n6: }\n7: fn other() { x() }",
            "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA\n-----END PUBLIC KEY-----\n",
            "-----BEGIN CERTIFICATE-----\nMIIDdzCCAl+gAwIBAgIE\n-----END CERTIFICATE-----",
        ] {
            assert_untouched(text);
        }
    }

    #[test]
    fn provider_tokens_and_jwts() {
        let masked = [
            (format!("aws_key = {AWS}\n"), "aws_key = [REDACTED]\n"),
            ("id: ASIAIOSFODNN7EXAMPLE.".into(), "id: [REDACTED]."),
            (format!("token={GITHUB}"), "token=[REDACTED]"),
            (
                format!("Bearer gho_{}", "b".repeat(40)),
                "Bearer [REDACTED]",
            ),
            (
                format!("pat github_pat_{} end", "c".repeat(30)),
                "pat [REDACTED] end",
            ),
            (
                "key = sk-proj-aaaaaaaaaaaaaaaaaaaaaaaa".into(),
                "key = [REDACTED]",
            ),
            (format!("sk-ant-api03-{}", "x".repeat(30)), "[REDACTED]"),
            (format!("k: AIza{}", "A".repeat(35)), "k: [REDACTED]"),
            ("SLACK xoxb-1234567890-abcdef".into(), "SLACK [REDACTED]"),
            (format!("sk_test_{}", "a".repeat(24)), "[REDACTED]"),
            (format!("rk_live_{} x", "b".repeat(16)), "[REDACTED] x"),
            (
                "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJmYWtlIn0.c2lnbmF0dXJlZmFrZQ"
                    .into(),
                "Authorization: Bearer [REDACTED]",
            ),
        ];
        for (input, want) in masked {
            assert_eq!(redacted(&input), want, "{input:?}");
        }

        for untouched in [
            "AKIAIOSFODNN7EXAM AKIAiosfodnn7example XAKIAIOSFODNN7EXAMPLE".to_string(),
            format!(
                "ghp_{} ghx_{} github_pat_short",
                "a".repeat(20),
                "a".repeat(40)
            ),
            "sk-short-value-1234 task-aaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            format!("AIza{}", "A".repeat(20)),
            "xoxb-123 xoxz-1234567890-abcdef".to_string(),
            format!("sk_test_short pk_test_{}", "a".repeat(24)),
            "eyJhbGciOiJIUzI1NiJ9.short.sig".to_string(),
        ] {
            assert_untouched(&untouched);
        }
    }

    #[test]
    fn credentials_in_urls() {
        assert_eq!(
            redacted("DATABASE_URL: postgres://adus:hunter2@db:5432/adus"),
            "DATABASE_URL: postgres://adus:[REDACTED]@db:5432/adus"
        );
        assert_eq!(
            redacted("see https://user:p%40ss@example.com/x"),
            "see https://user:[REDACTED]@example.com/x"
        );
        assert_untouched("https://example.com:8443/path?a=b and git@github.com:org/repo.git");
        assert_untouched("postgres://user:${DB_PASSWORD}@db/app");
    }

    #[test]
    fn named_assignments_with_quoted_literals() {
        assert_eq!(
            redacted(r#"password = "correct-horse-battery""#),
            r#"password = "[REDACTED]""#
        );
        assert_eq!(
            redacted(r#"{"client_secret": "abcdefgh12345678"}"#),
            r#"{"client_secret": "[REDACTED]"}"#
        );
        assert_eq!(
            redacted("db:\n  DB_PASSWORD: 'longenoughvalue'\n"),
            "db:\n  DB_PASSWORD: '[REDACTED]'\n"
        );
        assert_eq!(
            redacted(r#"const apiKey := "abcdefghijk""#),
            r#"const apiKey := "[REDACTED]""#
        );
        assert_eq!(
            redacted(r#"opts.Private_Key = "aaaaaaaaaaaa"; auth_token: "bbbbbbbbbbbb""#),
            r#"opts.Private_Key = "[REDACTED]"; auth_token: "[REDACTED]""#
        );
    }

    #[test]
    fn named_assignments_on_env_style_lines() {
        assert_eq!(
            redacted("API_KEY=abcdefgh123\nexport DB_PASSWORD=s3cr3tvalue\nPORT=8000\n"),
            "API_KEY=[REDACTED]\nexport DB_PASSWORD=[REDACTED]\nPORT=8000\n"
        );
        // Tool line-number prefixes and CRLF endings.
        assert_eq!(
            redacted("3: SECRET_KEY=abcdefghij\r\n4> access_token=zzzzzzzzzz"),
            "3: SECRET_KEY=[REDACTED]\r\n4> access_token=[REDACTED]"
        );
    }

    #[test]
    fn values_that_are_not_secrets_are_never_redacted() {
        for text in [
            "password = get_password()",
            "password=get_password()",
            "api_key: ${API_KEY}",
            "API_KEY=${API_KEY}",
            "secret: <your-secret>",
            r#"secret = "<your-secret>""#,
            r#""password": """#,
            r#"password = "short""#,
            "PASSWORD=short",
            "token_count = 12",
            "commit 3f786850e387550fdab836ed7e6dc881de23001b",
            "id: 123e4567-e89b-12d3-a456-426614174000",
            "Never log the password or the secret in plain text.",
            "Reset your password: it must be at least 12 characters.",
            r#"password = "[REDACTED]""#,
            "API_KEY=[REDACTED]",
            // Keyword arguments and JSX props: indented, or code-shaped values.
            "conn = connect(\n    host=host,\n    password=password,\n    api_key=settings.API_KEY,\n)\n",
            "<Client\n  apiKey={process.env.API_KEY}\n/>",
            "api_key=settings.API_KEY,",
            "apiKey={process.env.API_KEY}",
            "client(password=settings.PASSWORD)",
            "12: password=password,",
        ] {
            assert_untouched(text);
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        let text = format!("token = \"{GITHUB}\"\nPASSWORD=abcdefghij\n");
        let once = redacted(&text);
        assert!(matches!(redact_secrets(&once), Cow::Borrowed(_)), "{once}");
    }
}
