use anyhow::Result;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, stdin, stdout};

/// Hard cap on one newline-delimited frame (CB-029). Generous — frames carry
/// base64 images for describe_image/search_vision — but finite: before this,
/// `read_line` buffered an unterminated line without bound, so a misbehaving
/// upstream could OOM the shared memory daemon with one frame. The stdin peer
/// is agentd (trusted parent); this is defense-in-depth, not a network gate.
const MAX_FRAME_BYTES: u64 = 32 * 1024 * 1024;

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
    /// A line that blew past `MAX_FRAME_BYTES` (CB-029). The oversized tail is
    /// drained so the NEXT frame parses cleanly; the daemon stays up.
    Oversized { bytes: u64 },
}

/// Read one bounded frame from any buffered reader. Extracted from the
/// transport so the cap + drain logic is unit-testable without stdin.
pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R, max_bytes: u64) -> Result<Frame> {
    let mut line = String::new();
    let n = {
        let mut limited = reader.take(max_bytes);
        let n = limited.read_line(&mut line).await?;
        n as u64
    };
    if n == 0 {
        return Ok(Frame::Eof);
    }
    if n >= max_bytes && !line.ends_with('\n') {
        // The cap cut the line mid-way. Drain the remainder in buffer-sized
        // steps (never accumulating it) up to and including the newline, so
        // the next read starts on a frame boundary.
        let mut drained = n;
        loop {
            let buf = reader.fill_buf().await?;
            if buf.is_empty() {
                break; // EOF inside the oversized line
            }
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                drained += (pos + 1) as u64;
                reader.consume(pos + 1);
                break;
            }
            let len = buf.len();
            drained += len as u64;
            reader.consume(len);
        }
        return Ok(Frame::Oversized { bytes: drained });
    }
    match serde_json::from_str(line.trim()) {
        Ok(v)  => Ok(Frame::Value(v)),
        Err(e) => Ok(Frame::ParseError(e)),
    }
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
    /// EOF, per-frame parse failures, and oversized frames are returned as
    /// `Frame` variants so the caller can keep the daemon alive (CB-010/029).
    pub async fn read(&mut self) -> Result<Frame> {
        read_frame(&mut self.reader, MAX_FRAME_BYTES).await
    }

    pub async fn write(&mut self, value: &Value) -> Result<()> {
        let mut buf = serde_json::to_string(value)?;
        buf.push('\n');
        self.writer.write_all(buf.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn oversized_frame_is_bounded_and_drained() {
        // One 100-byte "line" against a 32-byte cap, followed by a valid frame:
        // the oversized read must not buffer the whole line, and the NEXT frame
        // must parse cleanly (the drain found the newline boundary).
        let big = "x".repeat(100);
        let input = format!("{big}\n{{\"ok\":true}}\n");
        let mut reader = tokio::io::BufReader::new(Cursor::new(input.into_bytes()));

        match read_frame(&mut reader, 32).await.unwrap() {
            Frame::Oversized { bytes } => assert_eq!(bytes, 101, "full line incl. newline drained"),
            _ => panic!("expected Oversized"),
        }
        match read_frame(&mut reader, 32).await.unwrap() {
            Frame::Value(v) => assert_eq!(v["ok"], true),
            _ => panic!("next frame must parse cleanly after the drain"),
        }
        assert!(matches!(read_frame(&mut reader, 32).await.unwrap(), Frame::Eof));
    }

    #[tokio::test]
    async fn oversized_line_at_eof_without_newline() {
        let input = "y".repeat(50); // no trailing newline, cap 16
        let mut reader = tokio::io::BufReader::new(Cursor::new(input.into_bytes()));
        match read_frame(&mut reader, 16).await.unwrap() {
            Frame::Oversized { bytes } => assert_eq!(bytes, 50),
            _ => panic!("expected Oversized"),
        }
        assert!(matches!(read_frame(&mut reader, 16).await.unwrap(), Frame::Eof));
    }

    #[tokio::test]
    async fn normal_and_malformed_frames_unchanged() {
        let input = "{\"a\":1}\nnot json\n";
        let mut reader = tokio::io::BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        assert!(matches!(read_frame(&mut reader, 1024).await.unwrap(), Frame::Value(_)));
        assert!(matches!(read_frame(&mut reader, 1024).await.unwrap(), Frame::ParseError(_)));
        assert!(matches!(read_frame(&mut reader, 1024).await.unwrap(), Frame::Eof));
    }

    #[tokio::test]
    async fn frame_exactly_at_cap_with_newline_parses() {
        // "{\"a\":1}\n" is 8 bytes — cap of exactly 8 must still parse it.
        let input = "{\"a\":1}\n";
        let mut reader = tokio::io::BufReader::new(Cursor::new(input.as_bytes().to_vec()));
        match read_frame(&mut reader, 8).await.unwrap() {
            Frame::Value(v) => assert_eq!(v["a"], 1),
            _ => panic!("exact-cap frame with newline must parse"),
        }
    }
}
