//! The diagnostic log's one filter for secrets (ADR-0058).
//!
//! It replaces values that look like credentials with [`REDACTED`]: values
//! of fields with a secret's name, JWTs, Matrix access and refresh tokens,
//! `Bearer` credentials and recovery keys. Library text is free text, so this
//! is a filter, not a proof; Persephone's own code never puts a secret in a
//! record.

use regex::{Captures, Regex};
use serde_json::{Map, Value};
use std::sync::LazyLock;

/// What a removed value is replaced with.
pub const REDACTED: &str = "[redacted]";

/// Parts of a name that mark a secret wherever they appear in it.
const SECRET_PARTS: &[&str] = &[
    "password",
    "passphrase",
    "passwd",
    "secret",
    "token",
    "jwt",
    "credential",
    "authorization",
    "cookie",
    "recovery",
    "private",
];

/// Names that hold a secret's word but are not secrets.
const NOT_SECRET: &[&str] = &["state_key"];

/// Whether a field with this name holds a secret: it contains one of
/// [`SECRET_PARTS`], or one of its `_`, `-` or `.` parts is or ends with
/// `key` or `keys` (`key`, `frame_key`, `apikey`, but not `keyed`).
pub fn is_secret_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if NOT_SECRET.contains(&name.as_str()) {
        return false;
    }
    SECRET_PARTS.iter().any(|part| name.contains(part))
        || name
            .split(['_', '-', '.', ' '])
            .any(|part| part.ends_with("key") || part.ends_with("keys"))
}

/// `name: value`, `name=value` and `"name":"value"`, also inside Debug text
/// with escaped quotes. The name is checked with [`is_secret_name`].
static NAMED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?i)([A-Za-z0-9_.\-]*(?:password|passphrase|passwd|secret|token|jwt|credential|authorization|cookie|recovery|private|key)[A-Za-z0-9_.\-]*)"#,
        r#"(\\{0,2}["']?\s*[:=]\s*(?:Some\()?(?:bearer\s+)?)"#,
        r#"(\\{1,2}"(?:[^"\\]|\\[^"])*\\{1,2}"|"(?:[^"\\]|\\.)*"|'[^']*'|[^\s"'&,;}\])]+)"#,
    ))
    .expect("valid pattern")
});

/// Other credential shapes, replaced wherever they appear.
static SHAPES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        // A JWT: three base64url parts, the first a JSON object.
        (
            r"eyJ[A-Za-z0-9_\-]{2,}\.[A-Za-z0-9_\-]{2,}\.[A-Za-z0-9_\-]*",
            REDACTED,
        ),
        // Synapse-style access and refresh tokens, and Matrix
        // Authentication Service tokens.
        (r"\b(?:syt|syr|mat|mct)_[A-Za-z0-9_\-]{6,}", REDACTED),
        (r"(?i)\bbearer\s+[A-Za-z0-9._~+/=\-]+", "Bearer [redacted]"),
        // A recovery key: 48 base58 characters starting with `Es`, in
        // groups of four or not.
        (
            r"\bEs[1-9A-HJ-NP-Za-km-z]{2}(?:\s?[1-9A-HJ-NP-Za-km-z]{4}){11}\b",
            REDACTED,
        ),
    ]
    .into_iter()
    .map(|(pattern, with)| (Regex::new(pattern).expect("valid pattern"), with))
    .collect()
});

/// `text` with every credential it holds replaced, cut to `limit` bytes.
pub fn text(text: &str, limit: usize) -> String {
    // Bounded before the patterns run; the cut below is the real limit.
    let mut bounded = text.to_owned();
    super::cut(&mut bounded, limit.saturating_mul(2));
    let mut clean = NAMED
        .replace_all(&bounded, |found: &Captures<'_>| {
            let (name, between, value) = (&found[1], &found[2], &found[3]);
            if !is_secret_name(name) {
                return found[0].to_owned();
            }
            let quote = if value.contains('"') {
                let escapes = value.chars().take_while(|&c| c == '\\').collect::<String>();
                format!("{escapes}\"")
            } else {
                String::new()
            };
            format!("{name}{between}{quote}{REDACTED}{quote}")
        })
        .into_owned();
    for (shape, with) in SHAPES.iter() {
        if let std::borrow::Cow::Owned(replaced) = shape.replace_all(&clean, *with) {
            clean = replaced;
        }
    }
    super::cut(&mut clean, limit);
    clean
}

/// Nesting deeper than this is written as `[deep]`.
const MAX_DEPTH: usize = 8;
/// Arrays keep this many items at most.
const MAX_ITEMS: usize = 64;

/// A JSON value with its strings cleaned by [`text`] and the values of
/// members with a secret's name replaced.
pub fn value(value: &Value, depth: usize) -> Value {
    if depth >= MAX_DEPTH {
        return Value::from("[deep]");
    }
    match value {
        Value::String(text) => Value::from(self::text(text, super::MAX_STRING_BYTES)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(MAX_ITEMS)
                .map(|item| self::value(item, depth + 1))
                .collect(),
        ),
        Value::Object(members) => {
            let mut clean = Map::new();
            for (name, item) in members.iter().take(MAX_ITEMS) {
                let name = self::text(name, 128);
                let item = if is_secret_name(&name) {
                    Value::from(REDACTED)
                } else {
                    self::value(item, depth + 1)
                };
                clean.insert(name, item);
            }
            Value::Object(clean)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(input: &str) -> String {
        text(input, 4096)
    }

    /// GIVEN field names THEN secrets' names are found and ordinary names,
    /// such as a state key or a flag named `keyed`, are not.
    #[test]
    fn given_names_then_secrets_are_found() {
        for name in [
            "password",
            "access_token",
            "accessToken",
            "refresh_token",
            "jwt",
            "key",
            "frame_key",
            "Authorization",
            "recovery_key",
            "client_secret",
            "keys",
        ] {
            assert!(is_secret_name(name), "{name}");
        }
        for name in [
            "state_key",
            "keyed",
            "room_id",
            "sender",
            "identity",
            "event_id",
        ] {
            assert!(!is_secret_name(name), "{name}");
        }
    }

    /// GIVEN credentials in the usual shapes of library text THEN each is
    /// replaced and the text around it stays.
    #[test]
    fn given_credentials_in_text_then_replaced() {
        let cases = [
            (
                "GET /sync?access_token=abc123&since=s1",
                "abc123",
                "since=s1",
            ),
            (
                r#"{"access_token":"opaque-1","expires_in":3600}"#,
                "opaque-1",
                "expires_in",
            ),
            ("password: \"hunter2\" user: @a:b", "hunter2", "@a:b"),
            ("token=Zm9vYmFy, next", "Zm9vYmFy", "next"),
            ("access_token: Some(\"tok-9\")", "tok-9", "access_token"),
            (
                r#"body: "{\"access_token\":\"escaped-1\"}""#,
                "escaped-1",
                "body",
            ),
            (
                // A made-up value, split so that scanners do not take it
                // for a real credential.
                concat!("header Authorization: Bear", "er abc.def-ghi"),
                "abc.def-ghi",
                "header",
            ),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJAYTpiIn0.sig_part ok",
                "eyJhbGci",
                "ok",
            ),
            (
                "using syt_YWxpY2U_RandomPart123_0aBcDe now",
                "syt_YWxpY2U",
                "now",
            ),
            (
                "key EsTc 5rr1 4Jhp Uc18 hwCn 2b9T LSvj 5h4T TkP8 bdeK JGTa Q2wx end",
                "EsTc 5rr1",
                "end",
            ),
        ];
        for (input, secret, kept) in cases {
            let output = clean(input);
            assert!(!output.contains(secret), "{input} -> {output}");
            assert!(output.contains(kept), "{input} -> {output}");
        }
    }

    /// GIVEN text with IDs and ordinary words THEN it is unchanged.
    #[test]
    fn given_ordinary_text_then_unchanged() {
        for input in [
            "joined !room:example.org as @alice:example.org on DEVICEID",
            "event $Abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG in room",
            "state_key: \"_@alice:example.org_DEVICE\"",
            "keyed=true reachable=false",
            "E2EE state for participant xyz: Ok",
        ] {
            assert_eq!(clean(input), input);
        }
    }

    /// GIVEN a JSON value with secret members at any depth THEN they are
    /// replaced and other members kept.
    #[test]
    fn given_json_then_secret_members_replaced() {
        let input = serde_json::json!({
            "user_id": "@a:b",
            "nested": [{ "refresh_token": "r1", "note": concat!("Bear", "er zzz") }],
        });
        let output = value(&input, 0);
        assert_eq!(output["user_id"], "@a:b");
        assert_eq!(output["nested"][0]["refresh_token"], REDACTED);
        assert_eq!(output["nested"][0]["note"], "Bearer [redacted]");
    }

    /// GIVEN long text THEN the result is cut to the limit.
    #[test]
    fn given_long_text_then_cut() {
        let output = text(&"a".repeat(10_000), 100);
        assert!(output.len() <= 100 + '…'.len_utf8());
    }
}
