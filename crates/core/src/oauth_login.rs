//! Sign-in through the homeserver's OAuth 2.0 provider (OIDC, ADR-0030), as
//! a native app (RFC 8252).
//!
//! * The core registers Persephone with the provider dynamically, as a
//!   native client whose only redirect is a loopback address on this
//!   computer: `http://127.0.0.1:<port>/callback`, the port chosen by the
//!   system for this attempt.
//! * It asks in its own window before the provider's page opens in the
//!   user's browser, naming the provider's host (as links do, ADR-0024).
//! * It waits for one request on the loopback address, at most
//!   [`REDIRECT_TIMEOUT`] and [`MAX_REQUEST_BYTES`]; only `GET /callback`
//!   with a query is taken, and the SDK checks its state against the one it
//!   sent. The browser gets a short page to close.
//! * No password is entered in Persephone: the provider asks for it.

use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// Longest the core waits for the browser to come back.
pub const REDIRECT_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Most bytes read of the redirect request.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
/// The page the browser shows once it is redirected back.
const DONE_PAGE: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\n\
    Cache-Control: no-store\r\nConnection: close\r\n\r\n\
    Persephone received the sign-in. You can close this window.\r\n";
const REFUSED_PAGE: &str = "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\n\
    Connection: close\r\n\r\nThis is not a sign-in answer.\r\n";

/// Why the redirect did not arrive.
#[derive(Debug, PartialEq, Eq)]
pub enum RedirectError {
    /// The user did not finish in time.
    TimedOut,
    /// The session began to lock (or another attempt replaced this one).
    Abandoned,
    /// The loopback address could not be used.
    Failed,
}

/// How often the wait checks whether it was abandoned.
const ABANDON_CHECK: Duration = Duration::from_millis(500);

/// A loopback listener for one redirect.
pub struct Redirect {
    listener: TcpListener,
    port: u16,
}

impl Redirect {
    /// Binds `127.0.0.1` on a port the system chooses.
    pub async fn bind() -> Result<Self, RedirectError> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| RedirectError::Failed)?;
        let port = listener
            .local_addr()
            .map_err(|_| RedirectError::Failed)?
            .port();
        Ok(Self { listener, port })
    }

    /// The redirect URI to register and send.
    pub fn uri(&self) -> url::Url {
        url::Url::parse(&format!("http://127.0.0.1:{}/callback", self.port))
            .expect("a loopback URL with a port parses")
    }

    /// Waits for the browser's redirect and returns its query. Requests
    /// that are not `GET /callback?…` are answered and ignored, until the
    /// time is up or `abandoned` says to stop, which is checked every
    /// [`ABANDON_CHECK`] so a lock is never held up for long.
    pub async fn wait(self, abandoned: impl Fn() -> bool) -> Result<String, RedirectError> {
        let watch = async {
            loop {
                tokio::time::sleep(ABANDON_CHECK).await;
                if abandoned() {
                    return;
                }
            }
        };
        tokio::select! {
            query = self.receive() => query,
            () = watch => Err(RedirectError::Abandoned),
        }
    }

    async fn receive(self) -> Result<String, RedirectError> {
        tokio::time::timeout(REDIRECT_TIMEOUT, async {
            loop {
                let (mut stream, _) = self
                    .listener
                    .accept()
                    .await
                    .map_err(|_| RedirectError::Failed)?;
                let mut request = Vec::new();
                let mut buffer = [0u8; 2048];
                while !request.windows(4).any(|end| end == b"\r\n\r\n")
                    && request.len() < MAX_REQUEST_BYTES
                {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => request.extend_from_slice(&buffer[..read]),
                    }
                }
                match callback_query(&request) {
                    Some(query) => {
                        let _ = stream.write_all(DONE_PAGE.as_bytes()).await;
                        return Ok(query);
                    }
                    None => {
                        let _ = stream.write_all(REFUSED_PAGE.as_bytes()).await;
                    }
                }
            }
        })
        .await
        .map_err(|_| RedirectError::TimedOut)?
    }
}

/// The query of a `GET /callback?<query> HTTP/1.x` request line, or `None`
/// for anything else.
pub fn callback_query(request: &[u8]) -> Option<String> {
    let line_end = request.windows(2).position(|end| end == b"\r\n")?;
    let line = std::str::from_utf8(&request[..line_end]).ok()?;
    let mut parts = line.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method != "GET" || !version.starts_with("HTTP/1.") || parts.next().is_some() {
        return None;
    }
    let query = target.strip_prefix("/callback?")?;
    (!query.is_empty() && query.bytes().all(|b| b.is_ascii_graphic())).then(|| query.to_owned())
}

/// The question asked before the provider's page opens: caption and
/// message, all printable ASCII. `homeserver` is the host signed in to.
pub fn question(homeserver: &str, url: &url::Url) -> Option<(&'static str, String)> {
    let provider = url.host_str()?;
    let shown = format!("{homeserver}{provider}");
    if url.scheme() != "https" || !shown.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    Some((
        "Sign in through your browser",
        format!(
            "{homeserver} signs you in through {provider}.\n\n\
             Persephone opens that page in your browser. Enter your password \
             there, never anywhere else, then come back here.\n\n\
             Open {provider} in your browser?"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GIVEN provider pages WHEN asked about THEN only https pages with a
    /// plain host get a question, naming both servers.
    #[test]
    fn given_provider_pages_then_only_https_ones_are_asked_about() {
        let page = url::Url::parse("https://auth.example.org/authorize?x=1").unwrap();
        let (caption, message) = question("example.org", &page).unwrap();
        assert_eq!(caption, "Sign in through your browser");
        assert!(message.starts_with("example.org signs you in through auth.example.org."));
        assert!(message.is_ascii());
        let plain = url::Url::parse("http://auth.example.org/").unwrap();
        assert!(question("example.org", &plain).is_none());
        assert!(question("ex\u{202E}ample.org", &page).is_none());
    }

    /// GIVEN a wait that is abandoned WHEN no redirect comes THEN it ends
    /// with `abandoned`.
    #[tokio::test]
    async fn given_an_abandoned_wait_then_it_ends() {
        let redirect = Redirect::bind().await.unwrap();
        assert_eq!(redirect.wait(|| true).await, Err(RedirectError::Abandoned));
    }

    /// GIVEN requests WHEN read THEN only `GET /callback` with a query gives
    /// one; other paths, methods, empty queries and odd lines give none.
    #[test]
    fn given_requests_then_only_the_callback_query_is_taken() {
        assert_eq!(
            callback_query(b"GET /callback?code=abc&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .as_deref(),
            Some("code=abc&state=xyz")
        );
        for bad in [
            &b"POST /callback?code=abc HTTP/1.1\r\n\r\n"[..],
            b"GET /other?code=abc HTTP/1.1\r\n\r\n",
            b"GET /callback? HTTP/1.1\r\n\r\n",
            b"GET /callback HTTP/1.1\r\n\r\n",
            b"GET /callback?code=a b HTTP/1.1\r\n\r\n",
            b"GET /callback?code=abc SPDY/3\r\n\r\n",
            b"GET /callback?code=abc HTTP/1.1",
            b"\xff\xfe\r\n\r\n",
        ] {
            assert_eq!(
                callback_query(bad),
                None,
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// GIVEN a listener WHEN a browser sends a stray request and then the
    /// callback THEN the stray one is refused and the callback's query is
    /// returned.
    #[tokio::test]
    async fn given_a_redirect_then_its_query_arrives_and_strays_are_refused() {
        let redirect = Redirect::bind().await.unwrap();
        let uri = redirect.uri();
        assert_eq!(uri.host_str(), Some("127.0.0.1"));
        assert_eq!(uri.path(), "/callback");
        let port = uri.port().unwrap();
        let browser = tokio::spawn(async move {
            let send = |request: &'static [u8]| async move {
                let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap();
                stream.write_all(request).await.unwrap();
                let mut answer = String::new();
                stream.read_to_string(&mut answer).await.unwrap();
                answer
            };
            let stray = send(b"GET /favicon.ico HTTP/1.1\r\n\r\n").await;
            let done = send(b"GET /callback?code=c&state=s HTTP/1.1\r\n\r\n").await;
            (stray, done)
        });
        assert_eq!(redirect.wait(|| false).await.unwrap(), "code=c&state=s");
        let (stray, done) = browser.await.unwrap();
        assert!(stray.starts_with("HTTP/1.1 400"));
        assert!(done.contains("You can close this window"));
    }
}
