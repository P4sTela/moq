use anyhow::{bail, Context};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;

#[allow(dead_code)]
struct RelayProcess {
    config: String,
    url: String,
    child: tokio::process::Child,
    log_path: PathBuf,
}

pub struct RelayManager {
    relays: Vec<RelayProcess>,
    results_dir: PathBuf,
    release: bool,
    moq_src: PathBuf,
    project_root: PathBuf,
}

impl RelayManager {
    pub fn new(results_dir: &str, release: bool) -> Self {
        // Discover moq-src directory by looking for Cargo.toml with workspace members
        let moq_src = find_moq_src().unwrap_or_else(|| PathBuf::from("moq-src"));
        let project_root = moq_src
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            relays: Vec::new(),
            results_dir: PathBuf::from(results_dir),
            release,
            moq_src,
            project_root,
        }
    }

    pub async fn kill_existing(&self) {
        let _ = Command::new("pkill")
            .args(["-9", "-f", "moq-relay"])
            .output()
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    pub async fn build(&self) -> anyhow::Result<()> {
        let mode = if self.release { "release" } else { "debug" };
        println!("  Building moq-relay ({})...", mode);

        let mut cmd = Command::new("cargo");
        cmd.args(["build", "--bin", "moq-relay"]);
        if self.release {
            cmd.arg("--release");
        }
        cmd.current_dir(&self.moq_src);

        let output = cmd.output().await.context("failed to run cargo build")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("cargo build failed (exit {}): {}", output.status, stderr);
        }
        println!("  Build complete");
        Ok(())
    }

    pub async fn start(&mut self, config: &str, url: &url::Url) -> anyhow::Result<()> {
        tokio::fs::create_dir_all(&self.results_dir).await?;

        // Resolve config path: try moq-src first, then project root
        let config_path = self.resolve_config(config)?;

        let log_name = config.replace(['/', '\\'], "-").replace(".toml", "");
        let log_path = self.results_dir.join(format!("relay-{}.log", log_name));

        let profile = if self.release { "release" } else { "debug" };
        let relay_bin = self.moq_src.join(format!("target/{}/moq-relay", profile));

        let log_file = std::fs::File::create(&log_path)
            .with_context(|| format!("failed to create log file: {}", log_path.display()))?;
        let log_stderr = log_file.try_clone()?;

        let child = Command::new(&relay_bin)
            .arg(&config_path)
            .current_dir(&self.moq_src)
            .env("RUST_LOG", "info")
            .env("MOQ_CLIENT_TLS_DISABLE_VERIFY", "true")
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_stderr))
            .spawn()
            .with_context(|| format!("failed to start relay: {}", relay_bin.display()))?;

        let pid = child.id().unwrap_or(0);
        self.relays.push(RelayProcess {
            config: config.to_string(),
            url: url.to_string(),
            child,
            log_path: log_path.clone(),
        });

        // Poll log file for "listening" (max 15s)
        for _ in 0..15 {
            // Check if process exited early
            if let Some(relay) = self.relays.last_mut() {
                if let Ok(Some(status)) = relay.child.try_wait() {
                    bail!(
                        "Relay exited early (code {}). Check {}",
                        status,
                        log_path.display()
                    );
                }
            }

            if let Ok(content) = tokio::fs::read_to_string(&log_path).await {
                if content.contains("listening") {
                    println!(
                        "  Relay started: {} -> {} (PID: {})",
                        config, url, pid
                    );
                    return Ok(());
                }
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        bail!(
            "Relay failed to start within 15s. Check {}",
            log_path.display()
        );
    }

    pub async fn stop_all(&mut self) {
        for relay in &mut self.relays {
            let _ = relay.child.kill().await;
        }
        self.relays.clear();
        // Also pkill to catch orphaned children
        let _ = Command::new("pkill")
            .args(["-9", "-f", "moq-relay"])
            .output()
            .await;
    }

    fn resolve_config(&self, config: &str) -> anyhow::Result<PathBuf> {
        // Try moq-src first (e.g. dev/root.toml)
        let path = self.moq_src.join(config);
        if path.exists() {
            return Ok(path);
        }

        // Try project root (e.g. configs/relay/root.toml)
        let path = self.project_root.join(config);
        if path.exists() {
            return Ok(path);
        }

        // Try as absolute/relative path
        let path = PathBuf::from(config);
        if path.exists() {
            return Ok(path);
        }

        bail!(
            "Relay config not found: {} (searched moq-src/ and project root)",
            config
        );
    }
}

/// Find the project root directory (parent of moq-src)
pub fn find_project_root() -> Option<PathBuf> {
    find_moq_src().and_then(|moq_src| moq_src.parent().map(|p| p.to_path_buf()))
}

/// Find the moq-src directory by searching upward from the executable
fn find_moq_src() -> Option<PathBuf> {
    // Try from current directory upward
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("moq-src");
        if candidate.join("Cargo.toml").exists() {
            return Some(candidate);
        }
        // Maybe we're inside moq-src
        if dir.join("Cargo.toml").exists() && dir.file_name().map(|n| n == "moq-src").unwrap_or(false) {
            return Some(dir);
        }
        // Also check if current dir IS moq-src (workspace root with rs/ directory)
        if dir.join("Cargo.toml").exists() && dir.join("rs").is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

impl Drop for RelayManager {
    fn drop(&mut self) {
        // Best-effort synchronous cleanup
        for relay in &mut self.relays {
            let _ = relay.child.start_kill();
        }
        // Also synchronous pkill
        let _ = std::process::Command::new("pkill")
            .args(["-9", "-f", "moq-relay"])
            .output();
    }
}
