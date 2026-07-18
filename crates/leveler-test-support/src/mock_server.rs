//! A minimal scriptable HTTP/1.1 server that mimics an OpenAI-compatible
//! provider. It serves one queued [`MockResponse`] per incoming connection.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

/// A scripted response for one connection.
#[derive(Debug, Clone)]
pub enum MockResponse {
    /// 200 `text/event-stream` written as one body, then a clean close.
    Sse { body: String },
    /// 200 stream written as separate raw byte chunks (to force fragmentation),
    /// each flushed with a small delay, then a clean close.
    RawChunks { chunks: Vec<Vec<u8>> },
    /// A non-2xx status with a JSON body.
    Status { code: u16, body: String },
}

impl MockResponse {
    /// A clean SSE stream: each frame becomes a `data: <frame>` event, followed
    /// by a terminal `[DONE]` sentinel.
    pub fn sse(frames: &[&str]) -> Self {
        let mut body = String::new();
        for f in frames {
            body.push_str("data: ");
            body.push_str(f);
            body.push_str("\n\n");
        }
        body.push_str("data: [DONE]\n\n");
        MockResponse::Sse { body }
    }

    /// An SSE stream that ends abruptly with no `[DONE]` and no finish event,
    /// simulating a mid-stream interruption.
    pub fn sse_interrupted(frames: &[&str]) -> Self {
        let mut body = String::new();
        for f in frames {
            body.push_str("data: ");
            body.push_str(f);
            body.push_str("\n\n");
        }
        MockResponse::Sse { body }
    }

    /// A raw byte stream split at arbitrary boundaries.
    pub fn fragmented(full_body: &str, chunk_size: usize) -> Self {
        let bytes = full_body.as_bytes();
        let chunks = bytes
            .chunks(chunk_size.max(1))
            .map(|c| c.to_vec())
            .collect();
        MockResponse::RawChunks { chunks }
    }

    /// A 200 response with a plain JSON body (for non-streaming `generate`).
    pub fn json_ok(body: &str) -> Self {
        MockResponse::Status {
            code: 200,
            body: body.to_string(),
        }
    }

    /// HTTP 429.
    pub fn too_many_requests() -> Self {
        MockResponse::Status {
            code: 429,
            body: r#"{"error":{"message":"rate limited"}}"#.to_string(),
        }
    }

    /// HTTP 500.
    pub fn server_error() -> Self {
        MockResponse::Status {
            code: 500,
            body: r#"{"error":{"message":"boom"}}"#.to_string(),
        }
    }
}

/// A running mock provider. Drop to stop it (the accept task is aborted).
pub struct MockServer {
    addr: SocketAddr,
    request_count: Arc<AtomicUsize>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl MockServer {
    /// Start a server that serves `responses` in order (repeating the last once
    /// exhausted) on `127.0.0.1` with an OS-assigned port.
    pub async fn start(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
        let request_count = Arc::new(AtomicUsize::new(0));

        let count = request_count.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let queue = queue.clone();
                let count = count.clone();
                tokio::spawn(async move {
                    let response = {
                        let mut q = queue.lock().await;
                        if q.len() > 1 {
                            q.pop_front()
                        } else {
                            q.front().cloned()
                        }
                    };
                    count.fetch_add(1, Ordering::SeqCst);
                    if let Some(resp) = response {
                        let _ = serve(stream, resp).await;
                    }
                });
            }
        });

        Self {
            addr,
            request_count,
            handle,
        }
    }

    /// Convenience: start with a single response repeated for all connections.
    pub async fn start_one(response: MockResponse) -> Self {
        Self::start(vec![response]).await
    }

    /// The base URL to point a provider at, e.g. `http://127.0.0.1:54321`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// How many requests have been received so far.
    pub fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

/// Read (and discard) the request, then write the scripted response.
async fn serve(mut stream: TcpStream, response: MockResponse) -> std::io::Result<()> {
    drain_request(&mut stream).await;

    match response {
        MockResponse::Sse { body } => {
            let header = "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: close\r\n\r\n";
            stream.write_all(header.as_bytes()).await?;
            stream.write_all(body.as_bytes()).await?;
            stream.flush().await?;
        }
        MockResponse::RawChunks { chunks } => {
            let header = "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/event-stream\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: close\r\n\r\n";
            stream.write_all(header.as_bytes()).await?;
            stream.flush().await?;
            for chunk in chunks {
                stream.write_all(&chunk).await?;
                stream.flush().await?;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        MockResponse::Status { code, body } => {
            let reason = reason_phrase(code);
            let header = format!(
                "HTTP/1.1 {code} {reason}\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).await?;
            stream.write_all(body.as_bytes()).await?;
            stream.flush().await?;
        }
    }
    stream.shutdown().await.ok();
    Ok(())
}

/// Read and discard the whole request — headers *and* the body indicated by
/// `Content-Length`. Draining only the headers (as an earlier version did) let
/// the server respond and close the socket while the client was still writing a
/// large body; the resulting RST surfaced in reqwest as "error decoding response
/// body". Small requests fit in the first TCP segments and happened to pass,
/// making the bug size-dependent and flaky. Reading the declared body length
/// first lets the client finish writing before we close.
async fn drain_request(stream: &mut TcpStream) {
    let mut buf = [0u8; 4096];
    let mut seen = Vec::new();
    let mut header_end = None;
    // 1. Read until the header terminator.
    while header_end.is_none() {
        let read = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await;
        match read {
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => {
                seen.extend_from_slice(&buf[..n]);
                header_end = seen
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4);
            }
            _ => return,
        }
    }
    // 2. Read the rest of the body declared by Content-Length, if any.
    let header_end = header_end.unwrap();
    let content_length = parse_content_length(&seen[..header_end]);
    let mut body_read = seen.len() - header_end;
    while body_read < content_length {
        let read = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await;
        match read {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => body_read += n,
            _ => break,
        }
    }
}

/// Parse a `Content-Length` header value (case-insensitive) from raw header
/// bytes; returns 0 when absent or unparsable.
fn parse_content_length(headers: &[u8]) -> usize {
    let text = String::from_utf8_lossy(headers);
    for line in text.split("\r\n") {
        if let Some((name, value)) = line.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            return value.trim().parse().unwrap_or(0);
        }
    }
    0
}

fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}
