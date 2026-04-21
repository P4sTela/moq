use crate::ipc::{self, ParentMessage, WorkerMessage};
use crate::metrics::MetricsSnapshot;
use anyhow::{bail, Context};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout};

const PHASE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct WorkerHandle {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    pub index: usize,
}

impl WorkerHandle {
    async fn send(&mut self, msg: &ParentMessage) -> anyhow::Result<()> {
        ipc::send_message(&mut self.stdin, msg).await
    }

    async fn recv(&mut self) -> anyhow::Result<Option<WorkerMessage>> {
        ipc::recv_message(&mut self.reader).await
    }
}

pub struct WorkerPool;

impl WorkerPool {
    /// Spawn N worker processes using the same binary with --worker-mode flag.
    pub async fn spawn(
        n: usize,
        tls_disable_verify: bool,
    ) -> anyhow::Result<Vec<WorkerHandle>> {
        let binary = std::env::current_exe().context("failed to get current executable path")?;
        let mut handles = Vec::with_capacity(n);

        for i in 0..n {
            let mut cmd = tokio::process::Command::new(&binary);
            cmd.arg("--worker-mode");
            if tls_disable_verify {
                cmd.arg("--tls-disable-verify");
            }
            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());

            let mut child = cmd.spawn().with_context(|| format!("failed to spawn worker {}", i))?;
            let stdin = child.stdin.take().expect("stdin piped");
            let stdout = child.stdout.take().expect("stdout piped");

            handles.push(WorkerHandle {
                child,
                stdin,
                reader: BufReader::new(stdout),
                index: i,
            });
        }

        eprintln!("  Spawned {} worker processes", n);
        Ok(handles)
    }

    /// Send connect commands to all workers and wait for connected responses.
    /// Returns the list of all connected client IDs across all workers.
    pub async fn connect_all(
        handles: &mut [WorkerHandle],
        relay_urls: &[url::Url],
        client_ids_per_worker: &[Vec<String>],
        publish_rate_hz: u32,
        payload_bytes: usize,
        stagger_delay_ms: u64,
    ) -> anyhow::Result<Vec<String>> {
        // Send connect to all workers
        for (handle, client_ids) in handles.iter_mut().zip(client_ids_per_worker.iter()) {
            let relay_url = relay_urls[handle.index % relay_urls.len()].to_string();
            handle
                .send(&ParentMessage::Connect {
                    relay_url,
                    client_ids: client_ids.clone(),
                    publish_rate_hz,
                    payload_bytes,
                    stagger_delay_ms,
                })
                .await?;
        }

        // Wait for connected responses from all workers
        let mut all_connected = Vec::new();
        for handle in handles.iter_mut() {
            let msg = tokio::time::timeout(PHASE_TIMEOUT, handle.recv())
                .await
                .context("timeout waiting for worker connect")?
                .context("failed to recv from worker")?;

            match msg {
                Some(WorkerMessage::Connected { client_ids }) => {
                    eprintln!(
                        "  Worker {} connected {} clients",
                        handle.index,
                        client_ids.len()
                    );
                    all_connected.extend(client_ids);
                }
                other => bail!(
                    "expected Connected from worker {}, got {:?}",
                    handle.index,
                    other
                ),
            }
        }

        Ok(all_connected)
    }

    /// Send subscribe command with all peer IDs to all workers, wait for ready.
    pub async fn subscribe_all(
        handles: &mut [WorkerHandle],
        all_peer_ids: &[String],
    ) -> anyhow::Result<()> {
        for handle in handles.iter_mut() {
            handle
                .send(&ParentMessage::Subscribe {
                    peer_ids: all_peer_ids.to_vec(),
                })
                .await?;
        }

        for handle in handles.iter_mut() {
            let msg = tokio::time::timeout(PHASE_TIMEOUT, handle.recv())
                .await
                .context("timeout waiting for worker subscribe")?
                .context("failed to recv from worker")?;

            match msg {
                Some(WorkerMessage::Ready) => {
                    eprintln!("  Worker {} subscribers ready", handle.index);
                }
                other => bail!(
                    "expected Ready from worker {}, got {:?}",
                    handle.index,
                    other
                ),
            }
        }

        Ok(())
    }

    /// Send publish command to all workers.
    pub async fn publish_all(handles: &mut [WorkerHandle]) -> anyhow::Result<()> {
        for handle in handles.iter_mut() {
            handle.send(&ParentMessage::Publish).await?;
        }
        Ok(())
    }

    /// Send stop command and collect metrics snapshots from all workers.
    pub async fn stop_and_collect(
        handles: &mut [WorkerHandle],
    ) -> anyhow::Result<Vec<MetricsSnapshot>> {
        for handle in handles.iter_mut() {
            handle.send(&ParentMessage::Stop).await?;
        }

        let mut snapshots = Vec::new();
        for handle in handles.iter_mut() {
            let msg = tokio::time::timeout(PHASE_TIMEOUT, handle.recv())
                .await
                .context("timeout waiting for worker metrics")?
                .context("failed to recv from worker")?;

            match msg {
                Some(WorkerMessage::Metrics { data }) => {
                    snapshots.push(data);
                }
                other => bail!(
                    "expected Metrics from worker {}, got {:?}",
                    handle.index,
                    other
                ),
            }

            // Wait for child process to exit
            let _ = handle.child.wait().await;
        }

        Ok(snapshots)
    }
}
