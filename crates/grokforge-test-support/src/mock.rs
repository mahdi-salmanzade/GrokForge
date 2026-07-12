//! A minimal, hand-rolled HTTP/1.1 mock of the xAI API.
//!
//! We deliberately avoid a framework (axum/wiremock) so tests get **byte-level control
//! over how the SSE body is chunked across TCP writes** — the only way to exercise the
//! client's handling of events split across reads. Each connection serves one request
//! then closes (`Connection: close`), and streaming bodies are delimited by EOF.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A request the mock received, captured for assertions (e.g. ledger byte reconciliation).
#[derive(Clone, Debug)]
pub struct Received {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Received {
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Exact number of bytes received in the request body — the ground truth the context
    /// ledger must reconcile against.
    #[must_use]
    pub fn body_len(&self) -> usize {
        self.body.len()
    }

    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// A scripted reply for a matched route.
#[derive(Clone, Debug)]
pub enum Reply {
    /// A complete non-streaming HTTP response (JSON endpoints, error statuses).
    Http {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    /// A streaming response whose body is delivered as explicit TCP chunks. Each chunk is
    /// written and flushed separately with a short gap so the client observes distinct reads.
    Stream {
        status: u16,
        headers: Vec<(String, String)>,
        chunks: Vec<Vec<u8>>,
    },
}

impl Reply {
    /// A JSON response with the given status.
    #[must_use]
    pub fn json(status: u16, value: &serde_json::Value) -> Self {
        Reply::Http {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: value.to_string().into_bytes(),
        }
    }

    /// A bare status code with an optional `Retry-After` header (for 429 tests).
    #[must_use]
    pub fn status_with_retry_after(status: u16, retry_after_secs: Option<u64>) -> Self {
        let mut headers = vec![("content-type".into(), "application/json".into())];
        if let Some(s) = retry_after_secs {
            headers.push(("retry-after".into(), s.to_string()));
        }
        Reply::Http {
            status,
            headers,
            body: br#"{"error":{"message":"scripted error"}}"#.to_vec(),
        }
    }

    /// An SSE stream where each `data:` frame is delivered as its own TCP chunk.
    #[must_use]
    pub fn sse_frames(frames: &[&str]) -> Self {
        let chunks = frames.iter().map(|f| f.as_bytes().to_vec()).collect();
        Reply::Stream {
            status: 200,
            headers: sse_headers(),
            chunks,
        }
    }

    /// An SSE stream built from JSON event values, each wrapped as `data: {json}\n\n`,
    /// one chunk per event.
    #[must_use]
    pub fn sse_events(events: &[serde_json::Value]) -> Self {
        let frames: Vec<String> = events.iter().map(|e| format!("data: {e}\n\n")).collect();
        let chunks = frames.into_iter().map(String::into_bytes).collect();
        Reply::Stream {
            status: 200,
            headers: sse_headers(),
            chunks,
        }
    }

    /// The same SSE body as [`Reply::sse_events`], but the full byte stream is re-split at the
    /// given byte offsets — used to prove the parser reassembles events fragmented across reads.
    #[must_use]
    pub fn sse_events_split_at(events: &[serde_json::Value], boundaries: &[usize]) -> Self {
        let mut all = Vec::new();
        for e in events {
            all.extend_from_slice(format!("data: {e}\n\n").as_bytes());
        }
        let mut chunks = Vec::new();
        let mut prev = 0usize;
        for &b in boundaries {
            let b = b.min(all.len());
            if b > prev {
                chunks.push(all[prev..b].to_vec());
                prev = b;
            }
        }
        if prev < all.len() {
            chunks.push(all[prev..].to_vec());
        }
        Reply::Stream {
            status: 200,
            headers: sse_headers(),
            chunks,
        }
    }
}

fn sse_headers() -> Vec<(String, String)> {
    vec![
        ("content-type".into(), "text/event-stream".into()),
        ("cache-control".into(), "no-cache".into()),
    ]
}

/// Builder for a [`MockXai`] server.
#[derive(Debug, Default)]
pub struct MockXaiBuilder {
    routes: HashMap<String, VecDeque<Reply>>,
}

impl MockXaiBuilder {
    /// Queue a reply for `POST`/`GET` requests to `path`. Multiple calls queue a sequence;
    /// once the queue has a single entry left, that entry repeats for every further request.
    #[must_use]
    pub fn route(mut self, path: &str, reply: Reply) -> Self {
        self.routes
            .entry(path.to_string())
            .or_default()
            .push_back(reply);
        self
    }

    /// Start the server on an ephemeral loopback port.
    pub async fn start(self) -> MockXai {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
        let addr = listener.local_addr().expect("addr");
        let received = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(Mutex::new(self.routes));

        let received_bg = Arc::clone(&received);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                let received = Arc::clone(&received_bg);
                let routes = Arc::clone(&routes);
                tokio::spawn(async move {
                    let _ = serve_conn(sock, received, routes).await;
                });
            }
        });

        MockXai {
            addr,
            received,
            _handle: handle,
        }
    }
}

/// A running mock server. Dropping it stops accepting new connections.
#[derive(Debug)]
pub struct MockXai {
    addr: SocketAddr,
    received: Arc<Mutex<Vec<Received>>>,
    _handle: tokio::task::JoinHandle<()>,
}

impl MockXai {
    #[must_use]
    pub fn builder() -> MockXaiBuilder {
        MockXaiBuilder::default()
    }

    /// Base URL to hand to the client under test, e.g. `http://127.0.0.1:PORT`.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// All requests received so far, in order.
    #[must_use]
    pub fn received(&self) -> Vec<Received> {
        self.received.lock().expect("lock").clone()
    }

    /// The most recent received request, if any.
    #[must_use]
    pub fn last_request(&self) -> Option<Received> {
        self.received.lock().expect("lock").last().cloned()
    }
}

type Routes = Arc<Mutex<HashMap<String, VecDeque<Reply>>>>;

async fn serve_conn(
    mut sock: tokio::net::TcpStream,
    received: Arc<Mutex<Vec<Received>>>,
    routes: Routes,
) -> std::io::Result<()> {
    // Read headers up to the blank line.
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let (method, path, headers) = parse_request_head(&buf[..header_end]);
    let content_length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    received.lock().expect("lock").push(Received {
        method,
        path: path.clone(),
        headers,
        body,
    });

    let reply = next_reply(&routes, &path);
    match reply {
        Some(Reply::Http {
            status,
            headers,
            body,
        }) => {
            let head = response_head(status, &headers, Some(body.len()));
            sock.write_all(head.as_bytes()).await?;
            sock.write_all(&body).await?;
            sock.flush().await?;
        }
        Some(Reply::Stream {
            status,
            headers,
            chunks,
        }) => {
            // No Content-Length: body is delimited by EOF (Connection: close).
            let head = response_head(status, &headers, None);
            sock.write_all(head.as_bytes()).await?;
            sock.flush().await?;
            for chunk in chunks {
                sock.write_all(&chunk).await?;
                sock.flush().await?;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        None => {
            let head = response_head(404, &[], Some(0));
            sock.write_all(head.as_bytes()).await?;
            sock.flush().await?;
        }
    }
    // Half-close so the client sees EOF on the body.
    sock.shutdown().await.ok();
    Ok(())
}

fn next_reply(routes: &Routes, path: &str) -> Option<Reply> {
    let mut guard = routes.lock().expect("lock");
    // Match by exact path, ignoring any query string.
    let key = path.split('?').next().unwrap_or(path).to_string();
    let queue = guard.get_mut(&key)?;
    if queue.len() > 1 {
        queue.pop_front()
    } else {
        queue.front().cloned()
    }
}

fn response_head(
    status: u16,
    headers: &[(String, String)],
    content_length: Option<usize>,
) -> String {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        401 => "Unauthorized",
        _ => "Status",
    };
    let mut out = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        out.push_str(k);
        out.push_str(": ");
        out.push_str(v);
        out.push_str("\r\n");
    }
    if let Some(len) = content_length {
        out.push_str("content-length: ");
        out.push_str(&len.to_string());
        out.push_str("\r\n");
    }
    out.push_str("connection: close\r\n\r\n");
    out
}

fn parse_request_head(head: &[u8]) -> (String, String, Vec<(String, String)>) {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    (method, path, headers)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
