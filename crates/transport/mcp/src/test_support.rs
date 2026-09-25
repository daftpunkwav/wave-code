/*!
 * @file McpHttpTestStub
 * @description In-crate HTTP/1.1 stub server shared by MCP test suites.
 *
 * Responsibilities:
 * - Serve scripted `(status, headers, body)` answers over a real loopback
 *   socket so HTTP client paths are exercised end to end.
 * - Record every request so tests can assert on paths and headers.
 *
 * This module is compiled only for tests (`cfg(test)` here, the
 * `test-support` feature downstream) and must never ship: it is raw
 * HTTP with no correctness guarantees beyond what the tests rely on.
 */

//! Hand-rolled HTTP stub for MCP client tests (loopback only).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// One request observed by the stub server.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// Request path (`/` when the request line carries none).
    pub path: String,
    /// Lower-cased header names as received.
    pub headers: HashMap<String, String>,
    /// Raw body bytes (Content-Length framed).
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// The JSON-RPC `method` field of the body, or an empty string when
    /// the body is not a JSON object carrying a method.
    pub fn rpc_method(&self) -> String {
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| {
                v.get("method")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default()
    }
}

/// Route handler: request in, `(status, extra headers, body)` out.
pub type StubHandler =
    Arc<dyn Fn(&RecordedRequest) -> (u16, Vec<(String, String)>, Vec<u8>) + Send + Sync>;

/// Minimal hand-rolled HTTP/1.1 stub (one connection per request, closed
/// after each response). No external network, no TLS, no framework.
pub struct StubServer {
    addr: std::net::SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl StubServer {
    /// Bind a loopback port and serve `handler` until dropped.
    pub async fn spawn(handler: StubHandler) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                let observed = observed.clone();
                tokio::spawn(async move {
                    serve_one(socket, &handler, &observed).await;
                });
            }
        });
        Self { addr, requests }
    }

    /// URL for one path on the stub (`""` gives the base URL).
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// The `name` header of the `index`th (0-based) observed request.
    pub fn header(&self, index: usize, name: &str) -> Option<String> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index)?
            .headers
            .get(name)
            .cloned()
    }

    /// How many requests the stub has served so far.
    pub fn request_count(&self) -> usize {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

/// Read one request, answer through `handler`, close the connection.
async fn serve_one(
    socket: tokio::net::TcpStream,
    handler: &StubHandler,
    observed: &Arc<Mutex<Vec<RecordedRequest>>>,
) {
    let (reader, mut writer) = socket.into_split();
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();
    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    parts.next();
    let path = parts.next().unwrap_or("/").to_owned();
    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_owned();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body).await.is_err() {
        return;
    }
    let request = RecordedRequest {
        path,
        headers,
        body,
    };
    observed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(request.clone());
    let (status, extra, resp_body) = handler(&request);
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp_body.len()
    );
    for (name, value) in &extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let _ = writer.write_all(head.as_bytes()).await;
    let _ = writer.write_all(&resp_body).await;
}
