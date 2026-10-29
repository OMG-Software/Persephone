//! Links in messages, opened in the user's browser only after the core asks
//! in its own window (ADR-0024).
//!
//! * The page never opens anything: it sends the link's text, and the core
//!   decides. A compromised page can therefore only ask; the user still sees
//!   the core's question with the full address.
//! * Only `http` and `https` links with a host and without a user name or
//!   password are accepted, at most [`MAX_LINK_BYTES`].
//! * The question shows the address as the `url` crate serializes it: an
//!   international host in its ASCII (punycode) form and other non-ASCII
//!   characters percent-encoded, so look-alike letters and bidirectional
//!   controls cannot disguise where the link goes. "No" is the default.

use crate::credentials::{CredentialSource, PromptError};
use persephone_core_protocol::ErrorCode;
use url::Url;

/// Longest link either side accepts, in bytes.
pub const MAX_LINK_BYTES: usize = 2048;

/// A character that does not belong in a link: a space, a control, or an
/// invisible or direction-changing character that could disguise one.
fn hidden(c: char) -> bool {
    c.is_control()
        || c.is_whitespace()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
        )
}

/// The link as the core would open it, or `None` when it is refused.
pub fn checked(raw: &str) -> Option<Url> {
    if raw.len() > MAX_LINK_BYTES || raw.chars().any(hidden) {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    let allowed = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none();
    allowed.then_some(url)
}

/// The question asked before a link opens: caption and message. Everything
/// shown is printable ASCII, from the serialized URL.
pub fn question(url: &Url) -> Result<(&'static str, String), PromptError> {
    let address = url.as_str();
    let host = url.host_str().ok_or(PromptError::Invalid)?;
    if !address.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(PromptError::Invalid);
    }
    let international = if host.split('.').any(|label| label.starts_with("xn--")) {
        "\n\nThe server's name uses international letters, shown here in \
         their plain form. Check that it is the site you expect: some \
         letters look alike."
    } else {
        ""
    };
    let plain = if url.scheme() == "http" {
        "\n\nThis link is not encrypted: anyone on the network can see and \
         change what the page shows."
    } else {
        ""
    };
    Ok((
        "Open this link?",
        format!(
            "{address}\n\nIt goes to the server {host}.{international}{plain}\n\n\
             Your browser connects to that server from this computer, so it \
             learns your IP address.\n\n\
             Open the link in your browser?"
        ),
    ))
}

/// Asks in the core's own window whether to open a checked link, then opens
/// it in the default browser: `true` when opened, `false` when the user said
/// no or closed the question. Nothing opens without an explicit yes.
pub fn open(source: &dyn CredentialSource, url: &Url) -> Result<bool, ErrorCode> {
    let (caption, message) = question(url).map_err(|_| ErrorCode::InvalidArgument)?;
    match source.confirm_open_link(caption, &message) {
        Ok(true) => {}
        Ok(false) | Err(PromptError::Cancelled) => return Ok(false),
        Err(PromptError::Unsupported) => return Err(ErrorCode::Unavailable),
        Err(_) => return Err(ErrorCode::Internal),
    }
    source
        .open_link(url.as_str())
        .map(|()| true)
        .map_err(|_| ErrorCode::Unavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::Credentials;
    use std::sync::Mutex;

    /// Answers the question with `answer` and records what it asked and
    /// opened.
    struct Asker {
        answer: Result<bool, PromptError>,
        asked: Mutex<Vec<String>>,
        opened: Mutex<Vec<String>>,
    }

    impl Asker {
        fn new(answer: Result<bool, PromptError>) -> Self {
            Self {
                answer,
                asked: Mutex::default(),
                opened: Mutex::default(),
            }
        }
    }

    impl CredentialSource for Asker {
        fn ask(&self, _: &str) -> Result<Credentials, PromptError> {
            Err(PromptError::Unsupported)
        }

        fn confirm_open_link(&self, _: &str, message: &str) -> Result<bool, PromptError> {
            self.asked.lock().unwrap().push(message.to_owned());
            self.answer
        }

        fn open_link(&self, url: &str) -> Result<(), PromptError> {
            self.opened.lock().unwrap().push(url.to_owned());
            Ok(())
        }
    }

    /// GIVEN each answer to the question WHEN a link is opened THEN it opens
    /// only after an explicit yes, always after being asked, and a platform
    /// without a question opens nothing.
    #[test]
    fn given_each_answer_then_a_link_opens_only_after_yes() {
        let url = checked("https://example.org/page").unwrap();
        for (answer, expected) in [
            (Ok(true), Ok(true)),
            (Ok(false), Ok(false)),
            (Err(PromptError::Cancelled), Ok(false)),
            (Err(PromptError::Failed), Err(ErrorCode::Internal)),
            (Err(PromptError::Unsupported), Err(ErrorCode::Unavailable)),
        ] {
            let asker = Asker::new(answer);
            assert_eq!(open(&asker, &url), expected, "{answer:?}");
            assert_eq!(asker.asked.lock().unwrap().len(), 1);
            let opened = asker.opened.lock().unwrap().clone();
            if expected == Ok(true) {
                assert_eq!(opened, ["https://example.org/page"]);
            } else {
                assert!(opened.is_empty(), "{answer:?} opened {opened:?}");
            }
        }
    }

    /// GIVEN links of each kind WHEN checked THEN only http and https links
    /// with a host and no credentials, without spaces or controls and within
    /// the limit, are accepted.
    #[test]
    fn given_links_then_only_plain_web_links_are_accepted() {
        for good in [
            "https://example.org/a?b=c#d",
            "http://example.org",
            "https://[2001:db8::1]/",
        ] {
            assert!(checked(good).is_some(), "{good}");
        }
        for bad in [
            "javascript:alert(1)",
            "file:///C:/Windows/System32/calc.exe",
            "mxc://example.org/abc",
            "data:text/html,<b>x</b>",
            "https://user:secret@example.org/",
            "https://user@example.org/",
            "https://",
            "https://example.org/a b",
            "https://example.org/\u{202E}",
            "ms-settings:privacy",
            "",
        ] {
            assert!(checked(bad).is_none(), "{bad:?}");
        }
        let long = format!("https://example.org/{}", "a".repeat(MAX_LINK_BYTES));
        assert!(checked(&long).is_none());
    }

    /// GIVEN a look-alike international host and a plain-HTTP link WHEN the
    /// question is made THEN the address shows in ASCII, punycode host
    /// included, with a warning for each, and nothing non-ASCII is shown.
    #[test]
    fn given_disguised_links_then_the_question_shows_their_plain_form() {
        // "аpple.com" with a Cyrillic "а".
        let url = checked("https://\u{0430}pple.com/login").unwrap();
        let (caption, message) = question(&url).unwrap();
        assert_eq!(caption, "Open this link?");
        assert!(
            message.starts_with("https://xn--pple-43d.com/login"),
            "{message}"
        );
        assert!(message.contains("server xn--pple-43d.com"));
        assert!(message.contains("international letters"));
        assert!(message.is_ascii());

        let (_, message) = question(&checked("http://example.org/caf\u{e9}").unwrap()).unwrap();
        assert!(message.contains("http://example.org/caf%C3%A9"));
        assert!(message.contains("not encrypted"));

        let (_, message) = question(&checked("https://example.org/").unwrap()).unwrap();
        assert!(!message.contains("international") && !message.contains("not encrypted"));
    }
}
