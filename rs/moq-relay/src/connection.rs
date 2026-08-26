use crate::{Auth, AuthError, AuthParams, AuthToken, Cluster};

use axum::http;
use moq_native::{Request, Transport};
use tracing::Instrument as _;

/// An error carrying the HTTP status to send when closing the request.
///
/// Used only on the pre-accept auth path so the caller can close once with
/// the right code instead of sprinkling close/return at each failure site.
struct StatusError {
	status: http::StatusCode,
	source: anyhow::Error,
}

impl From<AuthError> for StatusError {
	fn from(err: AuthError) -> Self {
		Self {
			status: (&err).into(),
			source: err.into(),
		}
	}
}

/// An incoming connection that has not yet been authenticated.
///
/// Call [`run`](Self::run) to authenticate the request, wire up
/// publish/subscribe origins, and serve the session until it closes.
pub struct Connection {
	/// A numeric identifier for logging.
	pub id: u64,
	/// The raw QUIC/WebTransport request to accept or reject.
	pub request: Request,
	/// The cluster state used to resolve origins.
	pub cluster: Cluster,
	/// The authenticator used to verify credentials.
	pub auth: Auth,
}

impl Connection {
	/// Authenticates and serves this connection until it closes.
	pub async fn run(self) -> anyhow::Result<()> {
		let id = self.id;
		async move { self.run_inner().await }
			.instrument(tracing::info_span!("conn", id))
			.await
	}

	async fn run_inner(self) -> anyhow::Result<()> {
		let peer_origin = self.request.peer_origin();
		let transport = self.request.transport();
		// URL-less transports must carry an explicit request target in SETUP. Without
		// one, Lite01-04 (which have no SETUP path) and pathless modern sessions would
		// silently authenticate against the root. WebSocket is intentionally excluded:
		// its URL is consumed by the WebSocket handshake and is not retained here.
		if self.request.url().is_none()
			&& matches!(
				transport,
				Transport::Quic | Transport::Iroh | Transport::Tcp | Transport::Unix
			) && self.request.path().is_empty()
		{
			let _ = self.request.close(http::StatusCode::BAD_REQUEST.as_u16()).await;
			anyhow::bail!("URL-less request is missing a SETUP path");
		}

		// URL-bearing transports carry the local control-only markers in the dial URL.
		// Raw QUIC and stream transports carry the same authenticated request target in
		// SETUP, so recover the markers from Request::path() before authorization.
		let (control_only, protocol_bytes_requested, remote_namespace, peer_url) = match self.request.url() {
			Some(url) => (
				crate::control_telemetry::query_flag(url, crate::control_telemetry::CONTROL_ONLY_QUERY),
				crate::control_telemetry::query_flag(url, crate::control_telemetry::PROTOCOL_BYTES_QUERY),
				crate::control_telemetry::query_value(url, crate::control_telemetry::NAMESPACE_QUERY),
				Some(crate::control_telemetry::sanitized_url(url)),
			),
			None => (
				crate::control_telemetry::query_flag_in_path(
					self.request.path(),
					crate::control_telemetry::CONTROL_ONLY_QUERY,
				),
				crate::control_telemetry::query_flag_in_path(
					self.request.path(),
					crate::control_telemetry::PROTOCOL_BYTES_QUERY,
				),
				crate::control_telemetry::query_value_in_path(
					self.request.path(),
					crate::control_telemetry::NAMESPACE_QUERY,
				),
				None,
			),
		};
		let token = match self.authenticate().await {
			Ok(token) => token,
			Err(err) => {
				let _ = self.request.close(err.status.as_u16()).await;
				return Err(err.source);
			}
		};

		let publish = self.cluster.publisher(&token);
		let subscribe = self.cluster.subscriber(&token);
		// The client advertises which direction it intends to use (moq-lite-05 SETUP).
		// A bidirectional connection (e.g. a cluster peer) advertises nothing, so the
		// only requirement is that the token grants *something*. But a gateway that only
		// publishes or only subscribes says so, and a token missing that direction's
		// scope is rejected here during the handshake, instead of being accepted and
		// then silently carrying no media (the bug that motivated the role hint).
		let role = self.request.role();
		let data_measurement =
			!control_only && (self.cluster.protocol_bytes_data_enabled() || protocol_bytes_requested);
		let protocol_bytes = if control_only || data_measurement {
			self.request.protocol_bytes()
		} else {
			None
		};
		if control_only {
			if protocol_bytes.is_none() {
				let _ = self
					.request
					.close(http::StatusCode::INTERNAL_SERVER_ERROR.as_u16())
					.await;
				anyhow::bail!("control-only session is missing protocol byte telemetry");
			}
			if !self.cluster.namespace_filter_enabled() {
				let _ = self.request.close(http::StatusCode::BAD_REQUEST.as_u16()).await;
				anyhow::bail!("control-only cluster sessions require namespace filtering");
			}
			let Some(remote_namespace) = remote_namespace.as_deref() else {
				let _ = self.request.close(http::StatusCode::BAD_REQUEST.as_u16()).await;
				anyhow::bail!("control-only cluster session is missing the namespace marker");
			};
			if let Err(error) = Cluster::validate_namespace(remote_namespace, "control-only namespace") {
				let _ = self.request.close(http::StatusCode::BAD_REQUEST.as_u16()).await;
				return Err(error);
			}
			if role.is_some() {
				let _ = self.request.close(http::StatusCode::BAD_REQUEST.as_u16()).await;
				anyhow::bail!("control-only cluster session must not advertise a data role");
			}
		} else if data_measurement && protocol_bytes.is_none() {
			let _ = self
				.request
				.close(http::StatusCode::INTERNAL_SERVER_ERROR.as_u16())
				.await;
			anyhow::bail!("data session is missing protocol byte telemetry");
		}
		let authorized = match role {
			Some(moq_net::Role::Publisher) => publish.is_some(),
			Some(moq_net::Role::Subscriber) => subscribe.is_some(),
			// Bidirectional or an unrecognized future role: require the token to grant
			// something, and let the per-direction checks apply once it's used.
			None | Some(_) => publish.is_some() || subscribe.is_some(),
		};
		if !authorized {
			let _ = self.request.close(http::StatusCode::FORBIDDEN.as_u16()).await;
			let wanted = role.map(|role| role.as_str()).unwrap_or("any");
			anyhow::bail!("token does not grant {wanted} access to {}", token.root);
		}

		if control_only {
			tracing::info!(
				%transport,
				?role,
				tier = %token.tier,
				root = %token.root,
				remote_namespace = ?remote_namespace,
				"control-only session accepted"
			);
		} else {
			match (&publish, &subscribe) {
				(Some(publish), Some(subscribe)) => {
					tracing::info!(%transport, ?role, tier = %token.tier, root = %token.root, publish = %publish.allowed().map(|p| p.as_str()).collect::<Vec<_>>().join(","), subscribe = %subscribe.allowed().map(|p| p.as_str()).collect::<Vec<_>>().join(","), "session accepted");
				}
				(Some(publish), None) => {
					tracing::info!(%transport, ?role, tier = %token.tier, root = %token.root, publish = %publish.allowed().map(|p| p.as_str()).collect::<Vec<_>>().join(","), "publisher accepted");
				}
				(None, Some(subscribe)) => {
					tracing::info!(%transport, ?role, tier = %token.tier, root = %token.root, subscribe = %subscribe.allowed().map(|p| p.as_str()).collect::<Vec<_>>().join(","), "subscriber accepted")
				}
				_ => unreachable!("authorized above guarantees at least one origin"),
			}
		}

		// Keep measurement model counters in a private registry so the artifact is
		// session-scoped. Normal data sessions additionally tee those counters into
		// the relay-wide billing registry; control-only sessions stay isolated.
		let telemetry_peer_url = peer_url.unwrap_or_else(|| format!("transport:{transport}"));
		let mut telemetry = None;
		let stats = if control_only {
			let model_registry = moq_net::stats::Registry::new(Default::default());
			let model_session = model_registry.tier(self.cluster.cluster_tier()).session("");
			telemetry = Some(self.cluster.control_sessions.begin(
				self.id,
				crate::control_telemetry::Direction::Inbound,
				telemetry_peer_url.clone(),
				remote_namespace.clone(),
				model_registry,
			));
			model_session
		} else if data_measurement {
			let model_registry = moq_net::stats::Registry::new(Default::default());
			let model_session = model_registry.tier(self.cluster.cluster_tier()).session("");
			let billing_session = self.cluster.stats.tier(token.tier.clone()).session(&token.root);
			telemetry = Some(self.cluster.data_sessions.begin_data(
				self.id,
				crate::control_telemetry::Direction::Inbound,
				telemetry_peer_url,
				remote_namespace,
				model_registry,
			));
			moq_net::stats::Session::tee(billing_session, model_session)
		} else {
			self.cluster.stats.tier(token.tier.clone()).session(&token.root)
		};

		// Wire only the direction(s) the client will actually use. The token scope
		// (enforced above) caps what it *may* do; the role caps what it *will* do.
		// Pruning the unused half means moq-net feeds that side a no-op origin, so a
		// publish-only ingest isn't announced every cluster broadcast it would ignore,
		// and a subscribe-only egress issues no announce-interest. A bidirectional
		// client (and any transport that carries no role) keeps whatever the token grants.
		let (publish, subscribe) = match role {
			Some(moq_net::Role::Publisher) => (publish, None),
			Some(moq_net::Role::Subscriber) => (None, subscribe),
			// Bidirectional or an unrecognized future role: keep whatever the token grants.
			None | Some(_) => (publish, subscribe),
		};

		// Accept the connection.
		// NOTE: subscribe and publish seem backwards because of how relays work.
		// We publish the tracks the client is allowed to subscribe to.
		// We subscribe to the tracks the client is allowed to publish.
		//
		// moq-net defaults the unset side to a fresh no-op origin, which is fine for a
		// publish-only or subscribe-only session. Control-only sessions intentionally
		// leave both sides unset.
		let mut request = self.request.with_stats(stats);
		if !control_only {
			if let Some(subscribe) = subscribe {
				request = request.with_publisher(&subscribe);
			}
			if let Some(publish) = publish {
				request = request.with_subscriber(publish);
			}
		}
		if let Some(telemetry) = &telemetry {
			telemetry.set_data_path_attached(request.has_data_path());
		}
		let session = match request.ok().await {
			Ok(session) => session,
			Err(error) => {
				if let Some(mut telemetry) = telemetry {
					telemetry.finish(crate::control_telemetry::State::Failed, Some(error.to_string()));
				}
				return Err(error.into());
			}
		};
		if let Some(telemetry) = &telemetry {
			telemetry.connected(&session);
		}
		let _node_connection = peer_origin.map(|origin| self.cluster.nodes.connect_inbound(self.id, origin));

		tracing::info!(version = %session.version(), %transport, "negotiated");

		// The credential (JWT `exp` or client cert `notAfter`) is only checked at
		// connect time, so hold the session open no longer than the credential is
		// valid. Without an expiry, just wait for the session to close.
		let result: anyhow::Result<()> = match token.expires {
			None => Err(session.closed().await.into()),
			Some(expires) => {
				let remaining = expires.duration_since(std::time::SystemTime::now()).unwrap_or_default();
				match tokio::time::timeout(remaining, session.closed()).await {
					Ok(err) => Err(err.into()),
					Err(_) => {
						tracing::info!("credential expired, closing session");
						session.abort(moq_net::Error::Unauthorized);
						Ok(())
					}
				}
			}
		};
		if let Some(mut telemetry) = telemetry {
			telemetry.finish(
				crate::control_telemetry::State::Closed,
				result.as_ref().err().map(|error| error.to_string()),
			);
		}
		result
	}

	/// Resolve an [`AuthToken`] for this connection. Any failure is returned as a
	/// [`StatusError`] so [`run`] can close the request with the mapped HTTP
	/// status exactly once.
	///
	/// Every transport goes through the same authenticator; only the source of
	/// the path + JWT differs:
	/// - URL-bearing transports (QUIC, WebSocket) take it from the request URL,
	///   and a valid mTLS client certificate (QUIC only) stands in for a JWT,
	///   granting full access within the URL path's root.
	/// - Stream transports (`tcp`/`unix`) take the path + `?jwt=` from the
	///   moq-lite-05 SETUP. A no-JWT connection resolves anonymous/public access
	///   for its path exactly like a tokenless QUIC client (`--auth-public`).
	///   Unix peer-credential gating happens earlier, in the listener.
	async fn authenticate(&self) -> Result<AuthToken, StatusError> {
		// Forwarded to the auth API so it can bucket by connection type (e.g. tier
		// the internal Unix-socket gateways separately). "quic"/"websocket"/"tcp"/
		// "unix"/"iroh".
		let transport = self.request.transport();
		let mut params = match self.request.url() {
			// URL-bearing transports: mTLS (QUIC only) can stand in for a JWT.
			Some(url) => {
				let params = self.auth.params_from_url(url);
				if let Some(identity) = self.request.peer_identity() {
					tracing::debug!("mTLS peer authenticated");
					// Scope the grant to the canonical root. An mTLS publisher dialing a
					// vanity alias lands on the same tree a JWT would; cluster peers dial
					// "/", which the API resolves (typically to an unscoped root). The API
					// also returns the billing tier.
					let mut token = self.auth.verify_mtls(&params.path, Some(transport)).await?;
					// Close the session when the client certificate expires, mirroring
					// the JWT `exp` handling. Validated once at the TLS handshake otherwise.
					token.expires = identity.expiry();
					return Ok(token);
				}
				params
			}
			// URL-less stream transports: path + `?jwt=` ride the SETUP.
			None => AuthParams::from_path(self.request.path()),
		};
		params.transport = Some(transport);

		Ok(self.auth.verify(&params).await?)
	}
}
