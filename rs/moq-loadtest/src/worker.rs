use crate::client::VirtualClient;
use crate::ipc::{self, ParentMessage, WorkerMessage};
use crate::metrics::Metrics;
use anyhow::Context;
use std::time::Duration;
use url::Url;

/// Run as a worker child process, communicating via stdin/stdout NDJSON.
/// All logging goes to stderr so stdout remains IPC-only.
pub async fn run_worker(quic_client: moq_native::Client) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = ipc::ndjson_reader(stdin);

    let metrics = Metrics::new();
    let mut clients: Vec<VirtualClient> = Vec::new();

    loop {
        let msg: Option<ParentMessage> = ipc::recv_message(&mut reader).await?;
        let msg = match msg {
            Some(m) => m,
            None => {
                // Parent closed stdin — exit gracefully
                break;
            }
        };

        match msg {
            ParentMessage::Connect {
                relay_url,
                client_ids,
                publish_rate_hz,
                payload_bytes,
                stagger_delay_ms,
            } => {
                let url: Url = relay_url.parse().context("invalid relay URL")?;
                let mut connected_ids = Vec::new();

                for (i, client_id) in client_ids.iter().enumerate() {
                    let mut vc = VirtualClient::new(
                        client_id.clone(),
                        metrics.clone(),
                        publish_rate_hz,
                        payload_bytes,
                    );

                    match vc.connect(url.clone(), &quic_client).await {
                        Ok(()) => {
                            eprintln!("  [worker] Connected {}", client_id);
                            connected_ids.push(client_id.clone());
                            clients.push(vc);
                        }
                        Err(e) => {
                            eprintln!("  [worker] Failed to connect {}: {}", client_id, e);
                        }
                    }

                    if i < client_ids.len() - 1 && stagger_delay_ms > 0 {
                        tokio::time::sleep(Duration::from_millis(stagger_delay_ms)).await;
                    }
                }

                ipc::send_message(
                    &mut stdout,
                    &WorkerMessage::Connected {
                        client_ids: connected_ids,
                    },
                )
                .await?;
            }

            ParentMessage::Subscribe { peer_ids } => {
                for client in &clients {
                    let peers: Vec<String> = peer_ids
                        .iter()
                        .filter(|id| *id != &client.id)
                        .cloned()
                        .collect();
                    client.start_subscribers(&peers);
                }
                // Brief pause for subscriber tasks to start listeners
                tokio::time::sleep(Duration::from_millis(100)).await;
                ipc::send_message(&mut stdout, &WorkerMessage::Ready).await?;
            }

            ParentMessage::Publish => {
                metrics.start(clients.len());
                for client in &mut clients {
                    client.start_publisher();
                }
            }

            ParentMessage::Stop => {
                for client in &clients {
                    client.stop();
                }
                // Brief wait for tasks to wind down
                tokio::time::sleep(Duration::from_millis(200)).await;

                let snapshot = metrics.snapshot();
                ipc::send_message(&mut stdout, &WorkerMessage::Metrics { data: snapshot })
                    .await?;
                break;
            }
        }
    }

    Ok(())
}
