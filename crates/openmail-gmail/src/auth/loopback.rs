//! A one-shot HTTP server on `127.0.0.1` that receives Google's OAuth redirect.
//!
//! The server listens on a random port, answers the single redirect request to `/`
//! with a small page telling the user to close the tab, and stops. Other paths get
//! a 404 and do not end the wait. Connections are read concurrently, so an idle
//! socket (a browser preconnect, say) cannot hold up the real redirect.

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio::time::timeout;
use url::form_urlencoded;

use super::AuthError;

/// The most bytes read from one request before giving up on it.
const MAX_REQUEST_BYTES: usize = 8 * 1024;
/// How long a connection may take to send its request line and headers.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

const SUCCESS_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>Openmail</title></head><body style=\"font-family: system-ui, sans-serif; \
text-align: center; margin-top: 4em\"><h1>Signed in to Openmail</h1>\
<p>You can close this tab and return to the app.</p></body></html>";

const FAILURE_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>Openmail</title></head><body style=\"font-family: system-ui, sans-serif; \
text-align: center; margin-top: 4em\"><h1>Sign-in did not complete</h1>\
<p>You can close this tab. Return to Openmail to see what went wrong and try again.</p>\
</body></html>";

const NOT_FOUND_PAGE: &str = "Not found";

/// A loopback listener waiting for one OAuth redirect.
#[derive(Debug)]
pub(crate) struct LoopbackServer {
    listener: TcpListener,
    redirect_uri: String,
}

impl LoopbackServer {
    /// Listens on `127.0.0.1` on a port chosen by the OS.
    pub(crate) async fn bind() -> Result<Self, AuthError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(AuthError::Loopback)?;
        let port = listener.local_addr().map_err(AuthError::Loopback)?.port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}/"),
        })
    }

    /// The `redirect_uri` to send in the authorization request.
    pub(crate) fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Waits for the redirect, answers it, and returns the authorization code.
    ///
    /// The listener is closed when this returns, whatever the outcome.
    ///
    /// # Errors
    ///
    /// [`AuthError::StateMismatch`] if the redirect's `state` is missing or differs
    /// from `expected_state`, [`AuthError::AuthorizationDenied`] if Google sent an
    /// `error`, [`AuthError::InvalidRedirect`] if there is no code, and
    /// [`AuthError::Loopback`] if accepting connections fails.
    pub(crate) async fn receive(self, expected_state: &str) -> Result<String, AuthError> {
        let mut pending = JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.listener.accept() => {
                    let (stream, _) = accepted.map_err(AuthError::Loopback)?;
                    pending.spawn(read_request(stream));
                }
                // Only an empty set may fail this pattern: a failed match disables
                // the branch until the next accept, which would strand a request
                // whose read finishes in the meantime.
                Some(joined) = pending.join_next() => {
                    let Ok(Some((mut stream, target))) = joined else {
                        continue;
                    };
                    let Some(query) = callback_query(&target) else {
                        respond(&mut stream, "404 Not Found", "text/plain", NOT_FOUND_PAGE).await;
                        continue;
                    };
                    let outcome = parse_callback(query, expected_state);
                    let (status, page) = match outcome {
                        Ok(_) => ("200 OK", SUCCESS_PAGE),
                        Err(_) => ("400 Bad Request", FAILURE_PAGE),
                    };
                    respond(&mut stream, status, "text/html; charset=utf-8", page).await;
                    return outcome;
                }
            }
        }
    }
}

/// Reads one request's head and returns the stream with its request target, or
/// `None` if the connection closes, stalls, or does not send a `GET` request.
async fn read_request(mut stream: TcpStream) -> Option<(TcpStream, String)> {
    let head = timeout(READ_TIMEOUT, read_head(&mut stream)).await.ok()??;
    let target = request_target(&head)?.to_owned();
    Some((stream, target))
}

async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() >= MAX_REQUEST_BYTES {
            return None;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8(buf).ok()
}

/// The target of a `GET` request line, such as `/?code=…&state=…`.
fn request_target(head: &str) -> Option<&str> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    (method == "GET" && version.starts_with("HTTP/1.") && parts.next().is_none()).then_some(target)
}

/// The query string if `target` is the callback path `/`.
fn callback_query(target: &str) -> Option<&str> {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    (path == "/").then_some(query)
}

/// Validates the redirect's query string and extracts the authorization code.
fn parse_callback(query: &str, expected_state: &str) -> Result<String, AuthError> {
    let params: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    let single = |name: &str| -> Result<Option<&str>, AuthError> {
        let mut values = params
            .iter()
            .filter(|(k, _)| k == name)
            .map(|(_, v)| v.as_str());
        match (values.next(), values.next()) {
            (value, None) => Ok(value),
            (Some(_), Some(_)) => Err(AuthError::InvalidRedirect(format!("repeated {name:?}"))),
            (None, Some(_)) => unreachable!("a second value implies a first"),
        }
    };

    if single("state")? != Some(expected_state) {
        return Err(AuthError::StateMismatch);
    }
    if let Some(error) = single("error")? {
        return Err(AuthError::AuthorizationDenied(error.to_owned()));
    }
    match single("code")? {
        Some(code) if !code.is_empty() => Ok(code.to_owned()),
        _ => Err(AuthError::InvalidRedirect(
            "missing authorization code".to_owned(),
        )),
    }
}

async fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    );
    // The browser may already have gone; the outcome stands either way.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use tokio::task::JoinHandle;

    use super::*;

    const STATE: &str = "expected-state";

    fn addr(server: &LoopbackServer) -> SocketAddr {
        server.listener.local_addr().unwrap()
    }

    async fn start() -> (SocketAddr, JoinHandle<Result<String, AuthError>>) {
        let server = LoopbackServer::bind().await.unwrap();
        let addr = addr(&server);
        (addr, tokio::spawn(server.receive(STATE)))
    }

    /// Sends a raw `GET` and returns the full response.
    async fn get(addr: SocketAddr, target: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nHost: {addr}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn binds_a_random_loopback_port() {
        let server = LoopbackServer::bind().await.unwrap();
        let port = addr(&server).port();
        assert_ne!(port, 0);
        assert!(addr(&server).ip().is_loopback());
        assert_eq!(server.redirect_uri(), format!("http://127.0.0.1:{port}/"));
    }

    #[tokio::test]
    async fn returns_the_code_and_a_close_this_tab_page() {
        let (addr, server) = start().await;
        let response = get(
            addr,
            &format!("/?state={STATE}&code=4%2F0Abc-123&scope=email"),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        assert!(response.contains("You can close this tab"));
        assert_eq!(server.await.unwrap().unwrap(), "4/0Abc-123");
    }

    #[tokio::test]
    async fn handles_exactly_one_redirect_then_closes() {
        let (addr, server) = start().await;
        get(addr, &format!("/?state={STATE}&code=abc")).await;
        server.await.unwrap().unwrap();
        assert!(TcpStream::connect(addr).await.is_err());
    }

    #[tokio::test]
    async fn rejects_a_mismatched_state() {
        let (addr, server) = start().await;
        let response = get(addr, "/?state=forged&code=abc").await;
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{response}"
        );
        assert!(matches!(
            server.await.unwrap(),
            Err(AuthError::StateMismatch)
        ));
    }

    #[tokio::test]
    async fn rejects_a_missing_state() {
        let (addr, server) = start().await;
        get(addr, "/?code=abc").await;
        assert!(matches!(
            server.await.unwrap(),
            Err(AuthError::StateMismatch)
        ));
    }

    #[tokio::test]
    async fn rejects_a_repeated_state() {
        let (addr, server) = start().await;
        get(addr, &format!("/?state={STATE}&state=other&code=abc")).await;
        assert!(matches!(
            server.await.unwrap(),
            Err(AuthError::InvalidRedirect(_))
        ));
    }

    #[tokio::test]
    async fn reports_a_denied_authorization() {
        let (addr, server) = start().await;
        let response = get(addr, &format!("/?error=access_denied&state={STATE}")).await;
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request\r\n"),
            "{response}"
        );
        match server.await.unwrap() {
            Err(AuthError::AuthorizationDenied(error)) => assert_eq!(error, "access_denied"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn rejects_a_missing_code() {
        let (addr, server) = start().await;
        get(addr, &format!("/?state={STATE}")).await;
        assert!(matches!(
            server.await.unwrap(),
            Err(AuthError::InvalidRedirect(_))
        ));
    }

    #[tokio::test]
    async fn other_paths_get_a_404_and_keep_waiting() {
        let (addr, server) = start().await;
        let response = get(addr, "/favicon.ico").await;
        assert!(
            response.starts_with("HTTP/1.1 404 Not Found\r\n"),
            "{response}"
        );
        get(addr, &format!("/?state={STATE}&code=abc")).await;
        assert_eq!(server.await.unwrap().unwrap(), "abc");
    }

    #[tokio::test]
    async fn idle_and_garbage_connections_do_not_block_the_redirect() {
        let (addr, server) = start().await;
        let _idle = TcpStream::connect(addr).await.unwrap();
        let mut garbage = TcpStream::connect(addr).await.unwrap();
        garbage.write_all(b"POST / HTTP/1.1\r\n\r\n").await.unwrap();
        drop(garbage);
        get(addr, &format!("/?state={STATE}&code=abc")).await;
        let code = timeout(Duration::from_secs(5), server).await.unwrap();
        assert_eq!(code.unwrap().unwrap(), "abc");
    }

    #[tokio::test]
    async fn a_dropped_connection_does_not_strand_an_accepted_redirect() {
        let (addr, server) = start().await;
        // The browser's connection is accepted first, but its request arrives late.
        let mut browser = TcpStream::connect(addr).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Meanwhile a preconnect closes without sending anything.
        drop(TcpStream::connect(addr).await.unwrap());
        tokio::time::sleep(Duration::from_millis(50)).await;

        let request = format!("GET /?state={STATE}&code=abc HTTP/1.1\r\nHost: {addr}\r\n\r\n");
        browser.write_all(request.as_bytes()).await.unwrap();
        let code = timeout(Duration::from_secs(5), server).await;
        assert_eq!(
            code.expect("redirect was never answered").unwrap().unwrap(),
            "abc"
        );
    }

    #[test]
    fn parses_request_lines() {
        assert_eq!(
            request_target("GET /?a=b HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some("/?a=b")
        );
        assert_eq!(request_target("POST / HTTP/1.1\r\n\r\n"), None);
        assert_eq!(request_target("GET / SPDY\r\n\r\n"), None);
        assert_eq!(request_target("GET /\r\n\r\n"), None);
    }
}
