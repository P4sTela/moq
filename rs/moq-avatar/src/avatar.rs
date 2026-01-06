//! Avatar data types and track definitions for metaverse simulation
//!
//! This module defines the data structures for multi-track avatar communication:
//! - Position track: 3D coordinates (x, y, z) updated at high frequency
//! - State track: Avatar state (animation, status) updated at lower frequency

use chrono::Utc;
use serde::{Deserialize, Serialize};

/// 3D position data for avatar
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// Timestamp when this position was generated (for latency measurement)
    pub timestamp_ms: i64,
}

impl Position {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self {
            x,
            y,
            z,
            timestamp_ms: Utc::now().timestamp_millis(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap()
    }

    pub fn from_bytes(data: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(data)?)
    }

    /// Calculate latency from when this position was created
    pub fn latency_ms(&self) -> f64 {
        let now = Utc::now().timestamp_millis();
        (now - self.timestamp_ms) as f64
    }
}

/// Avatar state data (animation, status, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvatarState {
    /// Animation ID (e.g., "idle", "walking", "running", "waving")
    pub animation: String,
    /// Health/status value (0-100)
    pub status: u8,
    /// Custom data field for extensibility
    pub custom: Option<String>,
    /// Timestamp when this state was generated
    pub timestamp_ms: i64,
}

impl AvatarState {
    pub fn new(animation: &str, status: u8) -> Self {
        Self {
            animation: animation.to_string(),
            status,
            custom: None,
            timestamp_ms: Utc::now().timestamp_millis(),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap()
    }

    pub fn from_bytes(data: &[u8]) -> anyhow::Result<Self> {
        Ok(serde_json::from_slice(data)?)
    }

    pub fn latency_ms(&self) -> f64 {
        let now = Utc::now().timestamp_millis();
        (now - self.timestamp_ms) as f64
    }
}

/// Track names used in the avatar broadcast
pub mod tracks {
    pub const POSITION: &str = "position";
    pub const STATE: &str = "state";
}

/// Statistics for tracking latency measurements
#[derive(Default, Debug)]
pub struct LatencyStats {
    pub samples: Vec<f64>,
    pub min_ms: f64,
    pub max_ms: f64,
    pub total_ms: f64,
    pub count: u64,
}

impl LatencyStats {
    pub fn record(&mut self, latency_ms: f64) {
        self.samples.push(latency_ms);
        self.count += 1;
        self.total_ms += latency_ms;
        if self.count == 1 {
            self.min_ms = latency_ms;
            self.max_ms = latency_ms;
        } else {
            self.min_ms = self.min_ms.min(latency_ms);
            self.max_ms = self.max_ms.max(latency_ms);
        }
    }

    pub fn avg_ms(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total_ms / self.count as f64
        }
    }

    pub fn percentile(&self, p: f64) -> f64 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let mut sorted = self.samples.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let idx = ((p / 100.0) * (sorted.len() - 1) as f64).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    pub fn print_summary(&self, track_name: &str) {
        if self.count == 0 {
            println!("\n=== {} Latency Statistics ===", track_name);
            println!("No samples collected");
            return;
        }
        println!("\n=== {} Latency Statistics ===", track_name);
        println!("Samples:    {}", self.count);
        println!("Min:        {:.2} ms", self.min_ms);
        println!("Max:        {:.2} ms", self.max_ms);
        println!("Average:    {:.2} ms", self.avg_ms());
        println!("P50:        {:.2} ms", self.percentile(50.0));
        println!("P95:        {:.2} ms", self.percentile(95.0));
        println!("P99:        {:.2} ms", self.percentile(99.0));
        println!("================================\n");
    }
}
