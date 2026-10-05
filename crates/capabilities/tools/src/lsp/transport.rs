//! Byte transport layer: LSP `Content-Length` framing plus the transports a
//! client can ride (a spawned language-server child, a boxed dispatcher, and
//! a test-only in-memory duplex).

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

/// Encode one JSON-RPC message with an LSP `Content-Length` header.
pub(super) fn encode_message(body: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(body).expect("JSON Value serializes infallibly");
    let mut out = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
    out.extend_from_slice(&json);
    out
}

/// Decode the first frame in `buf`: returns the value plus total bytes
/// consumed, or `None` when the buffer holds no complete frame yet.
/// Test-only: the live path streams via [`read_frame`]; this pure decoder
/// backs the frame unit tests.
#[cfg(test)]
pub(super) fn parse_frame(buf: &[u8]) -> Option<(Value, usize)> {
    let head_end = find_header_end(buf)?;
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut length: Option<usize> = None;
    for line in head.split("\r\n") {
        let lower = line.to_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            length = rest.trim().parse().ok();
        }
    }
    let length = length?;
    let total = head_end + 4 + length;
    if buf.len() < total {
        return None;
    }
    let value = serde_json::from_slice(&buf[head_end + 4..total]).ok()?;
    Some((value, total))
}

/// Locate the `\r\n\r\n` header terminator (test-only helper).
#[cfg(test)]
pub(super) fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Write one framed message.
pub(super) async fn write_frame<W>(writer: &mut W, msg: &Value) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    writer.write_all(&encode_message(msg)).await?;
    writer.flush().await
}

/// Byte cap on a single header line: a malformed or hostile server must
/// not be able to grow the read buffer without bound while the body cap
/// (below) never triggers. LSP headers are tiny in practice; 64 KiB is
/// orders of magnitude above any legitimate `Content-Length` line.
pub(super) const MAX_HEADER_LINE_BYTES: usize = 64 * 1024;

/// Read one framed message (whole read bounded by `timeout`).
pub(super) async fn read_frame<R>(reader: &mut R, timeout: Duration) -> std::io::Result<Value>
where
    R: AsyncBufReadExt + Unpin,
{
    tokio::time::timeout(timeout, async {
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            // The `take` bounds one header line's allocation: a server that
            // keeps sending bytes without a newline stops at the cap instead
            // of growing the string until the timeout fires.
            let mut limited = (&mut *reader).take(MAX_HEADER_LINE_BYTES as u64);
            let n = limited.read_line(&mut line).await?;
            drop(limited);
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "LSP stream closed",
                ));
            }
            if n as usize == MAX_HEADER_LINE_BYTES && !line.ends_with('\n') {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("LSP header line exceeds the {MAX_HEADER_LINE_BYTES}-byte cap"),
                ));
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if trimmed.to_lowercase().starts_with("content-length:") {
                content_length = trimmed["content-length:".len()..].trim().parse().ok();
            }
        }
        let length = content_length.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "LSP frame missing Content-Length",
            )
        })?;
        // The length comes from the peer; refuse absurd declarations instead
        // of pre-allocating (same threat as the llm SSE buffer cap).
        const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
        if length > MAX_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("LSP frame Content-Length {length} exceeds the {MAX_FRAME_BYTES}-byte cap"),
            ));
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).await?;
        serde_json::from_slice(&body)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "LSP read timed out"))?
}

/// Byte transport for JSON-RPC messages.
#[async_trait::async_trait]
pub trait LspTransport: Send {
    /// Send one message.
    async fn send(&mut self, msg: &Value) -> std::io::Result<()>;
    /// Receive one message, bounded by `timeout`.
    async fn recv(&mut self, timeout: Duration) -> std::io::Result<Value>;
}

/// Stdio transport over a spawned language-server child process.
pub struct ChildLsp {
    /// Kept alive for the session; `kill_on_drop` reaps it on timeout/drop.
    #[allow(dead_code)]
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
}

impl ChildLsp {
    /// Spawn `server_command` with `cwd` as working directory. The command
    /// is split on ASCII whitespace (program plus argv, no shell quoting);
    /// stderr is discarded so a chatty server cannot block on a full pipe.
    /// `deny_env` plus the shared sensitive-shape fallback are stripped
    /// from the child's environment (same scrub as the shell tool): a
    /// `server_command` is model-facing input here, so its process must
    /// not inherit secrets.
    pub fn spawn(
        server_command: &str,
        cwd: &std::path::Path,
        deny_env: &[String],
    ) -> std::io::Result<Self> {
        let mut parts = server_command.split_whitespace();
        let program = parts.next().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "empty server_command")
        })?;
        // OS confinement (chain: bwrap -> Landlock -> seatbelt -> Windows
        // job object; opt-in via WAVECODE_SANDBOX_OS), mirroring the shell
        // tool: a `server_command` is model-facing input, so when
        // confinement was requested the server must never run unconfined.
        // Rewriting backends replace the command wholesale, so the stdio
        // and env scrubbing below land on the final confined command. Any
        // backend failure fails closed: the spawn errors out and the call
        // surfaces it instead of downgrading to an unconfined child.
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(parts).current_dir(cwd);
        let mut armed = if wavecode_sandbox::os_sandbox_enabled() {
            let profile = wavecode_sandbox::ConfinementProfile::for_shell(cwd);
            let backend = wavecode_sandbox::detect_backend();
            match backend.spawn_confined(cmd, &profile) {
                Ok(armed) => armed,
                Err(e) => {
                    return Err(std::io::Error::other(format!(
                        "OS sandbox confinement failed ({}): {e}",
                        backend.backend_name()
                    )));
                }
            }
        } else {
            wavecode_sandbox::ArmedSpawn::new(cmd)
        };
        armed
            .command()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        // Env scrubbing lands on the final command (post-confinement),
        // same contract as the shell tool.
        crate::shell_tool::strip_child_env(armed.command().as_std_mut(), deny_env);
        let mut child = armed.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("server stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("server stdout unavailable"))?;
        Ok(Self {
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout),
        })
    }
}

#[async_trait::async_trait]
impl LspTransport for ChildLsp {
    async fn send(&mut self, msg: &Value) -> std::io::Result<()> {
        write_frame(&mut self.stdin, msg).await
    }

    async fn recv(&mut self, timeout: Duration) -> std::io::Result<Value> {
        read_frame(&mut self.stdout, timeout).await
    }
}

/// Boxed LSP transport: lets the provider registry hold one client type while
/// production spawns [`ChildLsp`] and tests inject in-memory fakes.
pub struct AnyTransport(pub(super) Box<dyn LspTransport>);

impl AnyTransport {
    /// Spawn a real language server over stdio (environment scrubbed via
    /// `deny_env`, see [`ChildLsp::spawn`]).
    pub fn spawn(server_command: &str, cwd: &Path, deny_env: &[String]) -> std::io::Result<Self> {
        Ok(Self(Box::new(ChildLsp::spawn(
            server_command,
            cwd,
            deny_env,
        )?)))
    }
}

#[async_trait::async_trait]
impl LspTransport for AnyTransport {
    async fn send(&mut self, msg: &Value) -> std::io::Result<()> {
        self.0.send(msg).await
    }

    async fn recv(&mut self, timeout: Duration) -> std::io::Result<Value> {
        self.0.recv(timeout).await
    }
}

/// In-memory transport over a `tokio::io::duplex` stream (test-only:
/// lets tests drive `LspClient` against a fake in-process server).
#[cfg(test)]
pub struct DuplexLsp {
    stream: tokio::io::BufReader<tokio::io::DuplexStream>,
}

#[cfg(test)]
impl DuplexLsp {
    /// Wrap one end of a duplex pair; the other end feeds the fake server.
    pub fn new(stream: tokio::io::DuplexStream) -> Self {
        Self {
            stream: tokio::io::BufReader::new(stream),
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl LspTransport for DuplexLsp {
    async fn send(&mut self, msg: &Value) -> std::io::Result<()> {
        write_frame(&mut self.stream, msg).await
    }

    async fn recv(&mut self, timeout: Duration) -> std::io::Result<Value> {
        read_frame(&mut self.stream, timeout).await
    }
}
