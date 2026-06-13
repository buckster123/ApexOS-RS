use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, stdin, stdout};

/// Outcome of reading one newline-delimited frame from stdin.
///
/// CB-010: a malformed frame must NOT be fatal. We distinguish a genuine
/// stdin EOF (clean shutdown — break the loop) from a per-frame deserialization
/// failure (log + JSON-RPC parse error, keep serving). An IO error on the
/// underlying stream still propagates as `Err`.
pub enum Frame {
    /// A well-formed JSON-RPC value.
    Value(Value),
    /// stdin reached EOF — the client disconnected cleanly.
    Eof,
    /// A non-empty line that failed to parse as JSON. The daemon must stay up.
    ParseError(serde_json::Error),
}

/// Newline-delimited JSON over stdin/stdout — the MCP wire format.
pub struct StdioTransport {
    reader: BufReader<tokio::io::Stdin>,
    writer: tokio::io::Stdout,
}

impl StdioTransport {
    pub fn new() -> Self {
        Self {
            reader: BufReader::new(stdin()),
            writer: stdout(),
        }
    }

    /// Read one frame. `Err` is reserved for genuine IO failures on stdin;
    /// EOF and per-frame parse failures are returned as `Frame` variants so the
    /// caller can keep the daemon alive (CB-010).
    pub async fn read(&mut self) -> Result<Frame> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(Frame::Eof);
        }
        match serde_json::from_str(line.trim()) {
            Ok(v)  => Ok(Frame::Value(v)),
            Err(e) => Ok(Frame::ParseError(e)),
        }
    }

    pub async fn write(&mut self, value: &Value) -> Result<()> {
        let mut buf = serde_json::to_string(value)?;
        buf.push('\n');
        self.writer.write_all(buf.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }
}
