//! A `wiremock` responder that decodes Gmail batch requests and answers each
//! embedded call through a handler.

use std::sync::{Arc, Mutex};

use wiremock::{Request, Respond, ResponseTemplate};

/// The batch endpoint path.
pub const BATCH: &str = "/batch/gmail/v1";

/// One embedded call from a batch request.
#[derive(Debug, Clone)]
pub struct Part {
    /// `N` from `Content-ID: <item-N>`.
    pub id: usize,
    /// The part's MIME headers, as sent.
    pub mime_headers: String,
    /// The request line, such as `GET /gmail/v1/users/me/messages/a HTTP/1.1`.
    pub request_line: String,
    /// The embedded request's headers and body, after the request line.
    pub rest: String,
}

/// One received batch: the bearer token and the parts, in order.
#[derive(Debug, Clone)]
pub struct Call {
    pub token: String,
    pub content_type: String,
    pub parts: Vec<Part>,
}

impl Call {
    pub fn ids(&self) -> Vec<usize> {
        self.parts.iter().map(|p| p.id).collect()
    }
}

type Handler = dyn Fn(&Call, &Part) -> (u16, String) + Send + Sync;

/// Answers each part with `handler(call, part)` → `(status, json body)`, in
/// reverse order to check that results are matched by `Content-ID`.
#[derive(Clone)]
pub struct BatchResponder {
    calls: Arc<Mutex<Vec<Call>>>,
    handler: Arc<Handler>,
}

impl BatchResponder {
    pub fn new(handler: impl Fn(&Call, &Part) -> (u16, String) + Send + Sync + 'static) -> Self {
        Self {
            calls: Arc::default(),
            handler: Arc::new(handler),
        }
    }

    /// Every batch received so far.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl Respond for BatchResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let call = decode(request);
        let responses: Vec<_> = call
            .parts
            .iter()
            .rev()
            .map(|part| (part.id, (self.handler)(&call, part)))
            .collect();
        self.calls.lock().unwrap().push(call);
        encode(&responses)
    }
}

fn decode(request: &Request) -> Call {
    let content_type = request.headers["content-type"].to_str().unwrap().to_owned();
    let boundary = content_type
        .split("boundary=")
        .nth(1)
        .expect("a boundary")
        .to_owned();
    let token = request.headers["authorization"]
        .to_str()
        .unwrap()
        .trim_start_matches("Bearer ")
        .to_owned();
    let body = String::from_utf8(request.body.clone()).unwrap();
    assert!(
        body.ends_with(&format!("--{boundary}--\r\n")),
        "missing closing delimiter: {body}"
    );

    let parts = body
        .split(&format!("--{boundary}"))
        .skip(1)
        .filter(|chunk| !chunk.starts_with("--"))
        .map(|chunk| {
            let chunk = chunk.strip_prefix("\r\n").unwrap();
            let (mime_headers, http) = chunk.split_once("\r\n\r\n").unwrap();
            let id = mime_headers
                .lines()
                .find_map(|l| l.strip_prefix("Content-ID: <item-"))
                .and_then(|l| l.strip_suffix('>'))
                .expect("a Content-ID")
                .parse()
                .unwrap();
            let (request_line, rest) = http.split_once("\r\n").unwrap();
            Part {
                id,
                mime_headers: mime_headers.to_owned(),
                request_line: request_line.to_owned(),
                rest: rest.to_owned(),
            }
        })
        .collect();
    Call {
        token,
        content_type,
        parts,
    }
}

fn encode(responses: &[(usize, (u16, String))]) -> ResponseTemplate {
    let boundary = "batch_response_xyz";
    let mut body = String::new();
    for (id, (status, json)) in responses {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Type: application/http\r\nContent-ID: <response-item-{id}>\r\n\r\n\
             HTTP/1.1 {status} X\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{json}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    ResponseTemplate::new(200).set_body_raw(
        body.into_bytes(),
        &format!("multipart/mixed; boundary={boundary}"),
    )
}

/// A Gmail error body.
pub fn error_json(code: u16, reason: &str) -> String {
    serde_json::json!({
        "error": {
            "code": code,
            "message": format!("error {code}"),
            "errors": [{"domain": "global", "reason": reason, "message": "x"}]
        }
    })
    .to_string()
}
