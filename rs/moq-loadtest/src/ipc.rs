use crate::metrics::MetricsSnapshot;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParentMessage {
    Connect {
        relay_url: String,
        client_ids: Vec<String>,
        publish_rate_hz: u32,
        payload_bytes: usize,
        stagger_delay_ms: u64,
    },
    Subscribe {
        peer_ids: Vec<String>,
    },
    Publish,
    Stop,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerMessage {
    Connected {
        client_ids: Vec<String>,
    },
    Ready,
    Metrics {
        data: MetricsSnapshot,
    },
}

/// Send a JSON message followed by a newline (NDJSON format)
pub async fn send_message<W: AsyncWriteExt + Unpin, M: Serialize>(
    writer: &mut W,
    msg: &M,
) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(msg)?;
    line.push('\n');
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Receive a JSON message from a line-buffered reader. Returns None on EOF.
pub async fn recv_message<R: tokio::io::AsyncBufRead + Unpin, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
) -> anyhow::Result<Option<M>> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    let msg = serde_json::from_str(line.trim())?;
    Ok(Some(msg))
}

/// Create a BufReader suitable for NDJSON reading from a tokio AsyncRead
pub fn ndjson_reader<R: tokio::io::AsyncRead>(reader: R) -> BufReader<R> {
    BufReader::new(reader)
}
