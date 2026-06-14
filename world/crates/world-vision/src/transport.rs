use anyhow::Result;
use serde_json::Value;
use tokio::io::{stdin, stdout, AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Newline-delimited JSON over stdin/stdout — the MCP wire format.
///
/// Mirrors `cerebro-mcp/src/transport.rs`. **stdout carries JSON-RPC only**; all logs
/// go to stderr (see `main.rs`). One JSON object per line in, one per line out.
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

    pub async fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            anyhow::bail!("EOF on stdin");
        }
        Ok(serde_json::from_str(line.trim())?)
    }

    pub async fn write(&mut self, value: &Value) -> Result<()> {
        let mut buf = serde_json::to_string(value)?;
        buf.push('\n');
        self.writer.write_all(buf.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::new()
    }
}
