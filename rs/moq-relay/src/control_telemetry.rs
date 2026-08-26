//! Structured telemetry for explicitly measured inbound and outbound sessions.
//!
//! This is deliberately separate from the relay-wide model traffic registry:
//! experiments need a connection-scoped record that can say both what the transport
//! carried and whether a model data path was attached to that connection. Control-only
//! and normal data-session records are kept in separate registries and artifacts.

use std::{
	collections::{BTreeMap, VecDeque},
	sync::{Arc, Mutex},
};

use moq_net::{
	Session,
	stats::{Registry as ModelRegistry, Role},
};
use serde::Serialize;

pub(crate) const CONTROL_ONLY_QUERY: &str = "control_only";
pub(crate) const PROTOCOL_BYTES_QUERY: &str = "protocol_bytes";
pub(crate) const NAMESPACE_QUERY: &str = "namespace";

const SCHEMA_VERSION: u32 = 4;
const MAX_COMPLETED: usize = 32;

pub(crate) fn query_flag(url: &url::Url, key: &str) -> bool {
	url.query_pairs().any(|(name, value)| name == key && value == "true")
}

pub(crate) fn query_value(url: &url::Url, key: &str) -> Option<String> {
	url.query_pairs()
		.find_map(|(name, value)| (name == key).then(|| value.into_owned()))
}

/// Read a reserved query marker from a request target carried inside SETUP.
///
/// Raw QUIC and stream transports do not expose a URL to the server, but the
/// client includes the request target in the SETUP path. These markers are
/// still subject to the normal token/mTLS authorization performed by the
/// connection before control-only policy is accepted.
pub(crate) fn query_flag_in_path(path: &str, key: &str) -> bool {
	query_value_in_path(path, key).is_some_and(|value| value == "true")
}

pub(crate) fn query_value_in_path(path: &str, key: &str) -> Option<String> {
	let query = path.split_once('?')?.1;
	let query = query.split_once('#').map_or(query, |(query, _)| query);
	url::form_urlencoded::parse(query.as_bytes()).find_map(|(name, value)| (name == key).then(|| value.into_owned()))
}

pub(crate) fn sanitized_url(url: &url::Url) -> String {
	let mut url = url.clone();
	url.set_query(None);
	url.set_fragment(None);
	url.to_string()
}

/// Cloneable registry shared by the cluster task and the trusted internal API.
#[derive(Clone, Default)]
pub(crate) struct Registry {
	inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
	active: BTreeMap<u64, Record>,
	completed: VecDeque<Snapshot>,
}

/// A handle held by one control-only session attempt.
pub(crate) struct Handle {
	registry: Registry,
	id: u64,
	finished: bool,
}

impl Registry {
	pub(crate) fn begin(
		&self,
		id: u64,
		direction: Direction,
		peer_url: String,
		remote_namespace: Option<String>,
		model_registry: ModelRegistry,
	) -> Handle {
		self.begin_kind(
			"control_only",
			id,
			direction,
			peer_url,
			remote_namespace,
			model_registry,
		)
	}

	pub(crate) fn begin_data(
		&self,
		id: u64,
		direction: Direction,
		peer_url: String,
		remote_namespace: Option<String>,
		model_registry: ModelRegistry,
	) -> Handle {
		self.begin_kind("data", id, direction, peer_url, remote_namespace, model_registry)
	}

	fn begin_kind(
		&self,
		kind: &'static str,
		id: u64,
		direction: Direction,
		peer_url: String,
		remote_namespace: Option<String>,
		model_registry: ModelRegistry,
	) -> Handle {
		let record = Record {
			connection_id: id,
			direction,
			kind,
			peer_url,
			remote_namespace,
			state: State::Connecting,
			version: None,
			session: None,
			model_registry,
			data_path_attached: None,
			error: None,
		};
		self.inner
			.lock()
			.expect("control telemetry registry poisoned")
			.active
			.insert(id, record);
		Handle {
			registry: self.clone(),
			id,
			finished: false,
		}
	}

	pub(crate) fn snapshot(&self) -> Telemetry {
		let inner = self.inner.lock().expect("control telemetry registry poisoned");
		let mut sessions = Vec::with_capacity(inner.active.len() + inner.completed.len());
		sessions.extend(inner.active.values().map(Record::snapshot));
		sessions.extend(inner.completed.iter().cloned());
		Telemetry {
			schema_version: SCHEMA_VERSION,
			sessions,
		}
	}

	fn connected(&self, id: u64, session: &Session) {
		let mut inner = self.inner.lock().expect("control telemetry registry poisoned");
		let Some(record) = inner.active.get_mut(&id) else {
			return;
		};
		record.state = State::Connected;
		record.version = Some(format!("{:?}", session.version()));
		record.session = Some(session.clone());
	}

	fn set_data_path_attached(&self, id: u64, attached: bool) {
		let mut inner = self.inner.lock().expect("control telemetry registry poisoned");
		if let Some(record) = inner.active.get_mut(&id) {
			record.data_path_attached = Some(attached);
		}
	}

	fn finish(&self, id: u64, state: State, error: Option<String>) {
		let Some(mut record) = self
			.inner
			.lock()
			.expect("control telemetry registry poisoned")
			.active
			.remove(&id)
		else {
			return;
		};
		record.state = state;
		record.error = error;
		let snapshot = record.snapshot();
		let mut inner = self.inner.lock().expect("control telemetry registry poisoned");
		inner.completed.push_back(snapshot);
		while inner.completed.len() > MAX_COMPLETED {
			inner.completed.pop_front();
		}
	}
}

impl Handle {
	pub(crate) fn connected(&self, session: &Session) {
		self.registry.connected(self.id, session);
	}

	pub(crate) fn set_data_path_attached(&self, attached: bool) {
		self.registry.set_data_path_attached(self.id, attached);
	}

	pub(crate) fn finish(&mut self, state: State, error: Option<String>) {
		if self.finished {
			return;
		}
		self.finished = true;
		self.registry.finish(self.id, state, error);
	}
}

impl Drop for Handle {
	fn drop(&mut self) {
		self.finish(State::Aborted, Some("dial attempt dropped".to_string()));
	}
}

struct Record {
	connection_id: u64,
	direction: Direction,
	kind: &'static str,
	peer_url: String,
	remote_namespace: Option<String>,
	state: State,
	version: Option<String>,
	session: Option<Session>,
	model_registry: ModelRegistry,
	/// Builder wiring observation; `None` until connect/accept inspects the request.
	data_path_attached: Option<bool>,
	error: Option<String>,
}

impl Record {
	fn snapshot(&self) -> Snapshot {
		let model_data = ModelData::from_registry(&self.model_registry);
		Snapshot {
			connection_id: self.connection_id,
			direction: self.direction,
			kind: self.kind,
			peer_url: self.peer_url.clone(),
			remote_namespace: self.remote_namespace.clone(),
			state: self.state,
			version: self.version.clone(),
			data_path_attached: self.data_path_attached,
			transport: Transport::from_session(self.session.as_ref()),
			moq_bytes: MoqBytes::from_session(self.session.as_ref()),
			model_data,
			error: self.error.clone(),
		}
	}
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
	Inbound,
	Outbound,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum State {
	Connecting,
	Connected,
	Closed,
	Failed,
	Aborted,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Telemetry {
	schema_version: u32,
	sessions: Vec<Snapshot>,
}

#[derive(Clone, Debug, Serialize)]
struct Snapshot {
	connection_id: u64,
	direction: Direction,
	kind: &'static str,
	peer_url: String,
	remote_namespace: Option<String>,
	state: State,
	version: Option<String>,
	/// Null for an in-flight record before the builder wiring is inspected.
	data_path_attached: Option<bool>,
	transport: Transport,
	moq_bytes: Option<MoqBytes>,
	model_data: ModelData,
	error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
struct Transport {
	bytes_sent: Option<u64>,
	bytes_received: Option<u64>,
	bytes_lost: Option<u64>,
	packets_sent: Option<u64>,
	packets_received: Option<u64>,
	packets_lost: Option<u64>,
}

impl Transport {
	fn from_session(session: Option<&Session>) -> Self {
		let Some(stats) = session.map(Session::stats) else {
			return Self::default();
		};
		Self {
			bytes_sent: stats.bytes_sent,
			bytes_received: stats.bytes_received,
			bytes_lost: stats.bytes_lost,
			packets_sent: stats.packets_sent,
			packets_received: stats.packets_received,
			packets_lost: stats.packets_lost,
		}
	}
}

#[derive(Clone, Debug, Serialize)]
struct MoqBytes {
	bytes_sent: u64,
	bytes_received: u64,
	control_bytes_sent: u64,
	control_bytes_received: u64,
	data_bytes_sent: u64,
	data_bytes_received: u64,
	unclassified_bytes_sent: u64,
	unclassified_bytes_received: u64,
}

impl MoqBytes {
	fn from_session(session: Option<&Session>) -> Option<Self> {
		session.and_then(Session::protocol_bytes).map(|stats| Self {
			bytes_sent: stats.bytes_sent,
			bytes_received: stats.bytes_received,
			control_bytes_sent: stats.control_bytes_sent,
			control_bytes_received: stats.control_bytes_received,
			data_bytes_sent: stats.data_bytes_sent,
			data_bytes_received: stats.data_bytes_received,
			unclassified_bytes_sent: stats.unclassified_bytes_sent,
			unclassified_bytes_received: stats.unclassified_bytes_received,
		})
	}
}

#[derive(Clone, Debug, Default, Serialize)]
struct ModelData {
	payload_bytes_sent: u64,
	payload_bytes_received: u64,
	groups_sent: u64,
	groups_received: u64,
	frames_sent: u64,
	frames_received: u64,
}

impl ModelData {
	fn from_registry(registry: &ModelRegistry) -> Self {
		let mut result = Self::default();
		for (_, role, traffic) in registry.snapshot().traffic() {
			match role {
				Role::Publisher => {
					result.payload_bytes_sent += traffic.bytes;
					result.groups_sent += traffic.groups;
					result.frames_sent += traffic.frames;
				}
				Role::Subscriber => {
					result.payload_bytes_received += traffic.bytes;
					result.groups_received += traffic.groups;
					result.frames_received += traffic.frames;
				}
			}
		}
		result
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn query_markers_are_decoded_and_credentials_are_removed() {
		let url = url::Url::parse(
			"https://peer.example/anon?jwt=secret&control_only=true&protocol_bytes=true&namespace=observation%2Fdiagonal",
		)
		.expect("parse test URL");
		assert!(query_flag(&url, CONTROL_ONLY_QUERY));
		assert!(query_flag(&url, PROTOCOL_BYTES_QUERY));
		assert_eq!(
			query_value(&url, NAMESPACE_QUERY).as_deref(),
			Some("observation/diagonal")
		);
		assert_eq!(sanitized_url(&url), "https://peer.example/anon");
	}

	#[test]
	fn setup_path_query_markers_are_decoded() {
		let path = "/anon?control_only=true&namespace=observation%2Fdiagonal#fragment";
		assert!(query_flag_in_path(path, CONTROL_ONLY_QUERY));
		assert_eq!(
			query_value_in_path(path, NAMESPACE_QUERY).as_deref(),
			Some("observation/diagonal")
		);
		assert!(!query_flag_in_path("/anon?control_only=false", CONTROL_ONLY_QUERY));
	}

	#[test]
	fn setup_path_without_query_has_no_markers() {
		assert!(!query_flag_in_path("/anon", CONTROL_ONLY_QUERY));
		assert_eq!(query_value_in_path("/anon", NAMESPACE_QUERY), None);
	}

	#[test]
	fn empty_registry_has_no_data_path() {
		let registry = Registry::default();
		let model = ModelRegistry::new(Default::default());
		let mut handle = registry.begin(
			42,
			Direction::Outbound,
			"https://peer.example/".to_string(),
			Some("observation/diagonal/leaf1-leaf4".to_string()),
			model,
		);
		let payload = serde_json::to_value(registry.snapshot()).expect("serialize telemetry");
		assert_eq!(payload["schema_version"], 4);
		assert_eq!(payload["sessions"][0]["direction"], "outbound");
		assert_eq!(payload["sessions"][0]["state"], "connecting");
		assert_eq!(payload["sessions"][0]["data_path_attached"], serde_json::Value::Null);
		handle.set_data_path_attached(false);
		let payload = serde_json::to_value(registry.snapshot()).expect("serialize telemetry");
		assert_eq!(payload["sessions"][0]["data_path_attached"], false);
		assert_eq!(payload["sessions"][0]["model_data"]["payload_bytes_sent"], 0);
		handle.finish(State::Failed, Some("test".to_string()));
		assert_eq!(registry.snapshot().sessions.len(), 1);
	}
}
