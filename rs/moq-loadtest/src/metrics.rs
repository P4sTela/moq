use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ConnectionRecord {
    pub client_id: String,
    pub success: bool,
    pub time_ms: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub latency_samples: Vec<f64>,
    pub publish_counts: Vec<(String, u64)>,
    pub receive_counts: Vec<(String, u64)>,
    pub connection_records: Vec<ConnectionRecord>,
}

pub struct Metrics {
    latency_samples: Mutex<Vec<f64>>,
    publish_counts: Mutex<HashMap<String, u64>>,
    receive_counts: Mutex<HashMap<String, u64>>,
    connection_records: Mutex<Vec<ConnectionRecord>>,
    start_time: Mutex<Option<Instant>>,
    connected_clients: Mutex<usize>,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            latency_samples: Mutex::new(Vec::new()),
            publish_counts: Mutex::new(HashMap::new()),
            receive_counts: Mutex::new(HashMap::new()),
            connection_records: Mutex::new(Vec::new()),
            start_time: Mutex::new(None),
            connected_clients: Mutex::new(0),
        })
    }

    pub fn start(&self, connected_clients: usize) {
        *self.start_time.lock().unwrap() = Some(Instant::now());
        *self.connected_clients.lock().unwrap() = connected_clients;
    }

    pub fn record_latency(&self, latency_ms: f64) {
        self.latency_samples.lock().unwrap().push(latency_ms);
    }

    pub fn record_publish(&self, client_id: &str) {
        let mut counts = self.publish_counts.lock().unwrap();
        *counts.entry(client_id.to_string()).or_insert(0) += 1;
    }

    pub fn record_receive(&self, publisher_id: &str, subscriber_id: &str) {
        let key = format!("{}->{}", publisher_id, subscriber_id);
        let mut counts = self.receive_counts.lock().unwrap();
        *counts.entry(key).or_insert(0) += 1;
    }

    pub fn record_connection(&self, client_id: &str, success: bool, time_ms: f64) {
        self.connection_records.lock().unwrap().push(ConnectionRecord {
            client_id: client_id.to_string(),
            success,
            time_ms,
        });
    }

    pub fn elapsed_seconds(&self) -> f64 {
        self.start_time
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }

    pub fn take_latency_samples(&self) -> Vec<f64> {
        std::mem::take(&mut *self.latency_samples.lock().unwrap())
    }

    pub fn take_publish_counts(&self) -> HashMap<String, u64> {
        std::mem::take(&mut *self.publish_counts.lock().unwrap())
    }

    pub fn take_receive_counts(&self) -> HashMap<String, u64> {
        std::mem::take(&mut *self.receive_counts.lock().unwrap())
    }

    pub fn take_connection_records(&self) -> Vec<ConnectionRecord> {
        std::mem::take(&mut *self.connection_records.lock().unwrap())
    }

    pub fn connected_clients(&self) -> usize {
        *self.connected_clients.lock().unwrap()
    }

    /// Non-destructive snapshot of all collected metrics
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            latency_samples: self.latency_samples.lock().unwrap().clone(),
            publish_counts: self
                .publish_counts
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            receive_counts: self
                .receive_counts
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            connection_records: self.connection_records.lock().unwrap().clone(),
        }
    }

    /// Merge an external MetricsSnapshot into this Metrics instance
    pub fn merge(&self, snapshot: MetricsSnapshot) {
        self.latency_samples
            .lock()
            .unwrap()
            .extend(snapshot.latency_samples);

        {
            let mut counts = self.publish_counts.lock().unwrap();
            for (k, v) in snapshot.publish_counts {
                *counts.entry(k).or_insert(0) += v;
            }
        }

        {
            let mut counts = self.receive_counts.lock().unwrap();
            for (k, v) in snapshot.receive_counts {
                *counts.entry(k).or_insert(0) += v;
            }
        }

        self.connection_records
            .lock()
            .unwrap()
            .extend(snapshot.connection_records);
    }
}
