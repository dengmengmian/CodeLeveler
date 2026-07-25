//! Everything the relay remembers — and, more importantly, everything it does not.
//!
//! The relay holds pairings, device records, routing tokens and live tunnel
//! registrations. It deliberately holds **no command queue and no transcript**.
//! That is a security decision, not a simplification: a queue would let a
//! command survive the revocation of the device that issued it, and a
//! transcript would put the user's session content on a machine the design
//! assumes may be compromised. When a host is offline the answer is 503, and
//! the device retries — the truth stays on the developer's machine.
//!
//! Tokens are opaque random strings looked up here, not self-contained JWTs.
//! The design permits either. This choice makes revocation a deletion, which
//! removes the need for an access-token denylist and with it a whole class of
//! "the denylist had a gap" bug. The cost is that horizontal scaling needs
//! shared storage for this table; that is a documented prerequisite rather than
//! something a second replica would silently get wrong.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use leveler_remote_protocol::SignedEnvelope;
use leveler_remote_protocol::auth::{
    ACCESS_TOKEN_TTL_SECS, REFRESH_TOKEN_TTL_SECS, SCOPE_REMOTE_SESSION, TokenClaims, TokenUse,
};
use leveler_remote_protocol::pairing::{PairingScope, PairingState};
use leveler_remote_protocol::tunnel::{RelayToAgent, RoutingError};

/// How long a pairing secret stays claimable.
pub const PAIRING_TTL_SECS: i64 = 5 * 60;
/// How long the host has to accept or reject after a device claims a secret.
pub const CONFIRM_TTL_SECS: i64 = 10 * 60;

/// A pairing in flight.
#[derive(Debug, Clone)]
pub struct Pairing {
    pub pairing_id: String,
    pub runtime_id: String,
    /// Stored hashed: a relay database dump must not hand out claimable secrets.
    pub secret_hash: [u8; 32],
    pub state: PairingState,
    pub created_at: i64,
    /// Set once a device claims the secret.
    pub claimed: Option<ClaimedBy>,
}

/// What a device presented when it claimed a pairing secret.
#[derive(Debug, Clone)]
pub struct ClaimedBy {
    pub device_id: String,
    pub device_pubkey: String,
    pub device_name: String,
    pub platform: String,
    pub scope: PairingScope,
    pub claimed_at: i64,
}

/// A device the host accepted. The relay keeps a copy for routing decisions;
/// the agent's own `devices.json` remains the authority for verification.
#[derive(Debug, Clone)]
pub struct DeviceRecord {
    pub device_id: String,
    pub runtime_id: String,
    pub device_pubkey: String,
    pub scope: PairingScope,
    pub revoked: bool,
}

/// A live token. Storing the claims rather than encoding them keeps one
/// definition of what a token asserts.
#[derive(Debug, Clone)]
pub struct TokenRecord {
    pub claims: TokenClaims,
}

/// Why a request was refused, with the wire code the design's catalogue names.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelayError {
    #[error("invalid pairing")]
    InvalidPairing,
    #[error("a pairing is already pending for this runtime")]
    AlreadyPending,
    #[error("not found")]
    NotFound,
    #[error("expired")]
    Expired,
    #[error("unauthorized")]
    Unauthorized,
    #[error("device revoked")]
    Revoked,
    #[error("refresh token reuse detected")]
    ReuseDetected,
    #[error("runtime is offline")]
    RuntimeOffline,
    #[error("too many attempts")]
    RateLimited,
}

impl RelayError {
    pub fn code(&self) -> &'static str {
        match self {
            RelayError::InvalidPairing => "invalid_pairing",
            RelayError::AlreadyPending => "already_pending",
            RelayError::NotFound => "not_found",
            RelayError::Expired => "expired",
            RelayError::Unauthorized => "unauthorized",
            RelayError::Revoked => "revoked",
            RelayError::ReuseDetected => "reuse_detected",
            RelayError::RuntimeOffline => "runtime_offline",
            RelayError::RateLimited => "rate_limited",
        }
    }

    pub fn http_status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            RelayError::InvalidPairing | RelayError::Expired => StatusCode::BAD_REQUEST,
            RelayError::AlreadyPending => StatusCode::CONFLICT,
            RelayError::NotFound => StatusCode::NOT_FOUND,
            RelayError::Unauthorized | RelayError::Revoked | RelayError::ReuseDetected => {
                StatusCode::UNAUTHORIZED
            }
            RelayError::RuntimeOffline => StatusCode::SERVICE_UNAVAILABLE,
            RelayError::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

/// The relay's whole memory. One process, one lock — adequate for a self-hosted
/// single-user relay and honest about not being more than that.
#[derive(Default)]
struct Inner {
    pairings: HashMap<String, Pairing>,
    devices: HashMap<String, DeviceRecord>,
    /// Opaque access token → claims.
    access: HashMap<String, TokenRecord>,
    /// Opaque refresh token → claims.
    refresh: HashMap<String, TokenRecord>,
    /// Refresh tokens already rotated away. A second use means the token was
    /// captured, so it costs the device every session it has.
    retired_refresh: HashMap<String, String>,
    /// Runtimes with a live agent tunnel. Absence is what makes a request 503
    /// rather than queueing it.
    online: HashMap<String, RuntimeOnline>,
    /// Live APP session streams, keyed by stream id.
    streams: HashMap<String, StreamRoute>,
    /// Runtime public keys, learned on first use and fixed thereafter.
    runtime_keys: HashMap<String, String>,
    /// `(subject_id, nonce)` already spent, with when — so a captured auth
    /// assertion cannot be replayed inside its own validity window.
    spent_nonces: HashMap<(String, String), i64>,
    /// Recent attempts per bucket, for rate limiting.
    attempts: HashMap<String, Vec<i64>>,
    /// RPCs waiting on an agent, with the runtime they were sent to. Held only
    /// for the life of one HTTP request — this is a correlation table, not a
    /// queue: nothing here survives the agent going away.
    pending_rpc: HashMap<String, PendingRpc>,
    counter: u64,
}

/// One in-flight RPC.
struct PendingRpc {
    runtime_id: String,
    answer: tokio::sync::oneshot::Sender<RpcOutcome>,
}

/// What an agent answered with: its runtime-signed envelope, or a routing-level
/// failure that carries no business body because the runtime produced no result.
#[derive(Debug, Clone)]
pub enum RpcOutcome {
    Signed(Box<SignedEnvelope>),
    Failed(RoutingError),
}

/// A registered, currently-connected runtime.
#[derive(Clone)]
pub struct RuntimeOnline {
    pub runtime_id: String,
    pub display_name: String,
    /// Frames queued for the agent's tunnel socket.
    pub to_agent: tokio::sync::mpsc::UnboundedSender<RelayToAgent>,
}

impl std::fmt::Debug for RuntimeOnline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeOnline")
            .field("runtime_id", &self.runtime_id)
            .field("display_name", &self.display_name)
            .finish_non_exhaustive()
    }
}

/// One live APP session stream.
#[derive(Clone)]
pub struct StreamRoute {
    pub stream_id: String,
    pub device_id: String,
    pub runtime_id: String,
    /// Frames queued for the device's socket.
    pub to_app: tokio::sync::mpsc::UnboundedSender<SignedEnvelope>,
}

/// Shared relay state.
#[derive(Clone, Default)]
pub struct RelayState {
    inner: Arc<Mutex<Inner>>,
    /// Hash of the operator's enrollment secret. `None` means no runtime can
    /// enroll — a relay with nothing configured accepts no hosts rather than
    /// accepting all of them.
    enrollment_secret_hash: Option<Arc<[u8; 32]>>,
}

impl RelayState {
    /// A relay that cannot enroll anything. Useful for tests of the parts that
    /// operate on already-enrolled runtimes; a real deployment wants
    /// [`Self::with_enrollment_secret`].
    pub fn new() -> Self {
        Self::default()
    }

    /// A relay an operator can enroll hosts into by presenting `secret`.
    pub fn with_enrollment_secret(secret: &str) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            enrollment_secret_hash: Some(Arc::new(sha256(secret.as_bytes()))),
        }
    }

    /// Register a runtime's public key against the operator's enrollment secret.
    ///
    /// This is the relay's only writer of `runtime_id → pubkey`. It is not
    /// trust-on-first-use: TOFU would hand the binding to whoever asked first,
    /// which on a reachable relay means anyone. The caller must present the
    /// secret the operator configured; the *signature* proving they hold the key
    /// is checked at the route, on the same request.
    ///
    /// Re-enrolling the same key is accepted so a host can retry; a different
    /// key for an id already bound is refused rather than treated as a rotation,
    /// because a silent key change is the attack. Rotating means removing the
    /// binding on the relay deliberately.
    pub fn enroll_runtime(
        &self,
        runtime_id: &str,
        pubkey: &str,
        presented_secret: &str,
    ) -> Result<(), RelayError> {
        let Some(expected) = self.enrollment_secret_hash.as_deref() else {
            return Err(RelayError::Unauthorized);
        };
        if !constant_time_eq(expected, &sha256(presented_secret.as_bytes())) {
            return Err(RelayError::Unauthorized);
        }

        let mut inner = self.inner.lock().unwrap();
        match inner.runtime_keys.get(runtime_id) {
            Some(existing) if existing == pubkey => Ok(()),
            Some(_) => Err(RelayError::Unauthorized),
            None => {
                inner
                    .runtime_keys
                    .insert(runtime_id.to_string(), pubkey.to_string());
                Ok(())
            }
        }
    }

    pub fn runtime_key(&self, runtime_id: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .runtime_keys
            .get(runtime_id)
            .cloned()
    }

    /// Spend a `(subject_id, nonce)` pair once, for either a device or a runtime
    /// assertion.
    ///
    /// A signed assertion is only good for one use; without this the signature
    /// would remain valid — and replayable — for its whole timestamp window.
    pub fn spend_nonce(&self, subject_id: &str, nonce: &str, now: i64) -> Result<(), RelayError> {
        let mut inner = self.inner.lock().unwrap();
        // Forget entries that can no longer be inside anyone's window, so the
        // map does not grow without bound.
        inner.spent_nonces.retain(|_, at| now - *at < 600);
        let key = (subject_id.to_string(), nonce.to_string());
        if inner.spent_nonces.contains_key(&key) {
            return Err(RelayError::Unauthorized);
        }
        inner.spent_nonces.insert(key, now);
        Ok(())
    }

    /// Allow at most `limit` attempts per `window_secs` for a bucket.
    ///
    /// Pairing secrets and auth assertions are the two things worth guessing, so
    /// both endpoints are metered.
    pub fn rate_limit(
        &self,
        bucket: &str,
        limit: usize,
        window_secs: i64,
        now: i64,
    ) -> Result<(), RelayError> {
        let mut inner = self.inner.lock().unwrap();
        let seen = inner.attempts.entry(bucket.to_string()).or_default();
        seen.retain(|at| now - *at < window_secs);
        if seen.len() >= limit {
            return Err(RelayError::RateLimited);
        }
        seen.push(now);
        Ok(())
    }

    /// Begin a pairing, returning the secret exactly once.
    ///
    /// Single-flight per runtime: a second `begin` cancels the first, so a
    /// stale QR left on a screen cannot be claimed later.
    pub fn begin_pairing(
        &self,
        runtime_id: &str,
        now: i64,
    ) -> Result<(String, String), RelayError> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .pairings
            .retain(|_, pairing| pairing.runtime_id != runtime_id);

        let secret = random_b64(32);
        inner.counter += 1;
        let pairing_id = format!("pair_{}", inner.counter);
        inner.pairings.insert(
            pairing_id.clone(),
            Pairing {
                pairing_id: pairing_id.clone(),
                runtime_id: runtime_id.to_string(),
                secret_hash: sha256(secret.as_bytes()),
                state: PairingState::PendingApp,
                created_at: now,
                claimed: None,
            },
        );
        Ok((pairing_id, secret))
    }

    /// A device claims a secret. Moves the pairing to awaiting host confirmation.
    ///
    /// The secret is compared in constant time and the failure is
    /// indistinguishable from "no such pairing", so the endpoint cannot be used
    /// to discover which secrets exist.
    pub fn claim_pairing(
        &self,
        secret: &str,
        claim: ClaimedBy,
        now: i64,
    ) -> Result<Pairing, RelayError> {
        let mut inner = self.inner.lock().unwrap();
        let presented = sha256(secret.as_bytes());

        let pairing = inner
            .pairings
            .values_mut()
            .find(|pairing| {
                pairing.state == PairingState::PendingApp
                    && constant_time_eq(&pairing.secret_hash, &presented)
            })
            .ok_or(RelayError::InvalidPairing)?;

        if now - pairing.created_at > PAIRING_TTL_SECS {
            pairing.state = PairingState::Expired;
            return Err(RelayError::Expired);
        }

        pairing.state = PairingState::PendingConfirm;
        pairing.claimed = Some(ClaimedBy {
            claimed_at: now,
            ..claim
        });
        Ok(pairing.clone())
    }

    /// The pairing this runtime is waiting to confirm, if any.
    pub fn pending_confirm_for(&self, runtime_id: &str) -> Option<Pairing> {
        let inner = self.inner.lock().unwrap();
        inner
            .pairings
            .values()
            .find(|pairing| {
                pairing.runtime_id == runtime_id && pairing.state == PairingState::PendingConfirm
            })
            .cloned()
    }

    /// The host accepted or rejected. Only an accept creates a device record.
    pub fn confirm_pairing(
        &self,
        pairing_id: &str,
        runtime_id: &str,
        accept: bool,
        now: i64,
    ) -> Result<(), RelayError> {
        let mut inner = self.inner.lock().unwrap();
        let pairing = inner
            .pairings
            .get_mut(pairing_id)
            .filter(|pairing| pairing.runtime_id == runtime_id)
            .ok_or(RelayError::NotFound)?;

        if pairing.state != PairingState::PendingConfirm {
            return Err(RelayError::NotFound);
        }
        let claimed = pairing.claimed.clone().ok_or(RelayError::NotFound)?;
        if now - claimed.claimed_at > CONFIRM_TTL_SECS {
            pairing.state = PairingState::Expired;
            return Err(RelayError::Expired);
        }

        if !accept {
            pairing.state = PairingState::Rejected;
            return Ok(());
        }

        pairing.state = PairingState::Active;
        let runtime_id = pairing.runtime_id.clone();
        inner.devices.insert(
            claimed.device_id.clone(),
            DeviceRecord {
                device_id: claimed.device_id,
                runtime_id,
                device_pubkey: claimed.device_pubkey,
                scope: claimed.scope,
                revoked: false,
            },
        );
        Ok(())
    }

    pub fn device(&self, device_id: &str) -> Option<DeviceRecord> {
        self.inner.lock().unwrap().devices.get(device_id).cloned()
    }

    pub fn devices_for(&self, runtime_id: &str) -> Vec<DeviceRecord> {
        self.inner
            .lock()
            .unwrap()
            .devices
            .values()
            .filter(|device| device.runtime_id == runtime_id)
            .cloned()
            .collect()
    }

    /// Withdraw a device's access.
    ///
    /// Every token it holds is dropped in the same breath, so revocation takes
    /// effect on the next request rather than whenever an access token would
    /// have expired.
    pub fn revoke_device(&self, device_id: &str, runtime_id: &str) -> Result<(), RelayError> {
        let mut inner = self.inner.lock().unwrap();
        let device = inner
            .devices
            .get_mut(device_id)
            .filter(|device| device.runtime_id == runtime_id)
            .ok_or(RelayError::NotFound)?;
        device.revoked = true;

        inner
            .access
            .retain(|_, record| record.claims.sub != device_id);
        inner
            .refresh
            .retain(|_, record| record.claims.sub != device_id);

        // Close its live streams in the same breath. Dropping only the tokens
        // would leave an already-open socket forwarding until it happened to
        // reconnect — exactly the "revoked but still delivering" case the threat
        // model calls out.
        let doomed: Vec<String> = inner
            .streams
            .values()
            .filter(|route| route.device_id == device_id)
            .map(|route| route.stream_id.clone())
            .collect();
        for stream_id in doomed {
            if let Some(route) = inner.streams.remove(&stream_id)
                && let Some(online) = inner.online.get(&route.runtime_id)
            {
                let _ = online.to_agent.send(RelayToAgent::CloseStream {
                    stream_id,
                    reason: "revoked".to_string(),
                });
            }
        }
        Ok(())
    }

    /// Mint an access/refresh pair for a paired, unrevoked device.
    pub fn issue_tokens(
        &self,
        device_id: &str,
        runtime_id: &str,
        now: i64,
    ) -> Result<(String, String, PairingScope), RelayError> {
        let mut inner = self.inner.lock().unwrap();
        let device = inner
            .devices
            .get(device_id)
            .ok_or(RelayError::Unauthorized)?;
        if device.revoked {
            return Err(RelayError::Revoked);
        }
        // A device is paired to one runtime. Asking for a token against another
        // is not an authorization question the relay should answer generously.
        if device.runtime_id != runtime_id {
            return Err(RelayError::Unauthorized);
        }
        let scope = device.scope;

        let (access, refresh) = mint_pair(&mut inner, device_id, runtime_id, scope, now);
        Ok((access, refresh, scope))
    }

    /// Rotate a refresh token.
    ///
    /// The old token is retired, and presenting a retired one is treated as
    /// theft rather than as a retry: every token the device holds is dropped and
    /// it has to authenticate again.
    pub fn rotate_refresh(
        &self,
        presented: &str,
        asserting_device: &str,
        now: i64,
    ) -> Result<(String, String), RelayError> {
        let mut inner = self.inner.lock().unwrap();

        if let Some(device_id) = inner.retired_refresh.get(presented).cloned() {
            inner.access.retain(|_, r| r.claims.sub != device_id);
            inner.refresh.retain(|_, r| r.claims.sub != device_id);
            return Err(RelayError::ReuseDetected);
        }

        let record = inner
            .refresh
            .remove(presented)
            .ok_or(RelayError::Unauthorized)?;
        if record.claims.exp <= now {
            return Err(RelayError::Expired);
        }
        // The assertion proves possession of *a* paired key; this is what ties it
        // to *this* token, so one device cannot spend another's captured one.
        if record.claims.sub != asserting_device {
            return Err(RelayError::Unauthorized);
        }
        let device_id = record.claims.sub.clone();
        let runtime_id = record.claims.aud.clone();
        let scope = record.claims.pairing_scope;

        match inner.devices.get(&device_id) {
            Some(device) if !device.revoked => {}
            Some(_) => return Err(RelayError::Revoked),
            None => return Err(RelayError::Unauthorized),
        }

        inner
            .retired_refresh
            .insert(presented.to_string(), device_id.clone());
        let (access, refresh) = mint_pair(&mut inner, &device_id, &runtime_id, scope, now);
        Ok((access, refresh))
    }

    /// Resolve a bearer token to its claims, checking the audience.
    ///
    /// `expected_runtime` is what makes a token for one host inert against
    /// another — the isolation the design requires between paired machines.
    pub fn authorize(
        &self,
        token: &str,
        expected_runtime: Option<&str>,
        now: i64,
    ) -> Result<TokenClaims, RelayError> {
        let inner = self.inner.lock().unwrap();
        let record = inner.access.get(token).ok_or(RelayError::Unauthorized)?;
        if record.claims.exp <= now {
            return Err(RelayError::Unauthorized);
        }
        if let Some(runtime_id) = expected_runtime
            && record.claims.aud != runtime_id
        {
            return Err(RelayError::Unauthorized);
        }
        match inner.devices.get(&record.claims.sub) {
            Some(device) if !device.revoked => Ok(record.claims.clone()),
            Some(_) => Err(RelayError::Revoked),
            None => Err(RelayError::Unauthorized),
        }
    }

    /// A runtime's agent tunnel came up.
    pub fn register_runtime(
        &self,
        runtime_id: &str,
        display_name: &str,
        to_agent: tokio::sync::mpsc::UnboundedSender<RelayToAgent>,
    ) {
        self.inner.lock().unwrap().online.insert(
            runtime_id.to_string(),
            RuntimeOnline {
                runtime_id: runtime_id.to_string(),
                display_name: display_name.to_string(),
                to_agent,
            },
        );
    }

    /// Send an RPC to a runtime's agent and hand back the receiver for its
    /// answer.
    ///
    /// The envelope goes through verbatim: it is the device's signature over
    /// the method, the project and the body, and the relay is only choosing
    /// which socket it leaves by.
    pub fn begin_rpc(
        &self,
        runtime_id: &str,
        envelope: SignedEnvelope,
    ) -> Result<tokio::sync::oneshot::Receiver<RpcOutcome>, RelayError> {
        let mut inner = self.inner.lock().unwrap();
        let online = inner
            .online
            .get(runtime_id)
            .cloned()
            .ok_or(RelayError::RuntimeOffline)?;

        inner.counter += 1;
        let rpc_id = format!("rpc_{}", inner.counter);
        let (answer, receiver) = tokio::sync::oneshot::channel();
        inner.pending_rpc.insert(
            rpc_id.clone(),
            PendingRpc {
                runtime_id: runtime_id.to_string(),
                answer,
            },
        );

        online
            .to_agent
            .send(RelayToAgent::RpcRequest { rpc_id, envelope })
            .map_err(|_| RelayError::RuntimeOffline)?;
        Ok(receiver)
    }

    /// An agent answered an RPC.
    ///
    /// Returns the outcome untouched when no request is waiting on that id, so
    /// the caller can decide what an unmatched answer means rather than having
    /// it silently swallowed here.
    pub fn complete_rpc(&self, rpc_id: &str, outcome: RpcOutcome) -> Option<RpcOutcome> {
        let pending = self.inner.lock().unwrap().pending_rpc.remove(rpc_id);
        match pending {
            Some(pending) => {
                let _ = pending.answer.send(outcome);
                None
            }
            None => Some(outcome),
        }
    }

    /// A runtime's agent tunnel went away.
    ///
    /// Its streams go with it and nothing is retained: no queue, so a command
    /// aimed at it while offline is refused rather than stored and replayed
    /// later, possibly after the device that sent it was revoked.
    pub fn unregister_runtime(&self, runtime_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.online.remove(runtime_id);
        inner
            .streams
            .retain(|_, route| route.runtime_id != runtime_id);
        // Dropping a pending sender ends its waiting request. The alternative —
        // holding it for a reconnect — is the durable queue this design refuses
        // to have.
        inner
            .pending_rpc
            .retain(|_, pending| pending.runtime_id != runtime_id);
    }

    /// Open an APP session stream and tell the agent about it.
    ///
    /// The `open_stream` frame carries no device public key on purpose: the
    /// agent resolves that from its own store. A key supplied here would let the
    /// relay choose what the agent trusts.
    #[allow(clippy::too_many_arguments)]
    pub fn open_stream(
        &self,
        device_id: &str,
        runtime_id: &str,
        project_id: Option<&str>,
        pairing_scope: PairingScope,
        access_jti: &str,
        to_app: tokio::sync::mpsc::UnboundedSender<SignedEnvelope>,
    ) -> Result<String, RelayError> {
        let mut inner = self.inner.lock().unwrap();
        let online = inner
            .online
            .get(runtime_id)
            .cloned()
            .ok_or(RelayError::RuntimeOffline)?;

        inner.counter += 1;
        let stream_id = format!("str_{}", inner.counter);
        inner.streams.insert(
            stream_id.clone(),
            StreamRoute {
                stream_id: stream_id.clone(),
                device_id: device_id.to_string(),
                runtime_id: runtime_id.to_string(),
                to_app,
            },
        );

        let _ = online.to_agent.send(RelayToAgent::OpenStream {
            stream_id: stream_id.clone(),
            device_id: device_id.to_string(),
            // Passed through as the device asked for it. The agent decides
            // whether that project exists; the relay holds no project truth.
            project_id: project_id.map(|id| id.to_string()),
            pairing_scope,
            access_jti: access_jti.to_string(),
        });
        Ok(stream_id)
    }

    /// Close a stream and tell the agent why.
    pub fn close_stream(&self, stream_id: &str, reason: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(route) = inner.streams.remove(stream_id)
            && let Some(online) = inner.online.get(&route.runtime_id)
        {
            let _ = online.to_agent.send(RelayToAgent::CloseStream {
                stream_id: stream_id.to_string(),
                reason: reason.to_string(),
            });
        }
    }

    /// Forward a device frame to its runtime, unchanged.
    ///
    /// The envelope is moved through verbatim. Unwrapping it to re-wrap the
    /// payload in JSON of the relay's own would strip the signature that makes
    /// it trustworthy, so this function only routes.
    pub fn forward_upstream(
        &self,
        stream_id: &str,
        frame: SignedEnvelope,
    ) -> Result<(), RelayError> {
        let inner = self.inner.lock().unwrap();
        let route = inner.streams.get(stream_id).ok_or(RelayError::NotFound)?;
        let online = inner
            .online
            .get(&route.runtime_id)
            .ok_or(RelayError::RuntimeOffline)?;
        online
            .to_agent
            .send(RelayToAgent::ForwardUpstream {
                stream_id: stream_id.to_string(),
                frame,
            })
            .map_err(|_| RelayError::RuntimeOffline)
    }

    /// Forward a runtime frame to the device that owns the stream, unchanged.
    pub fn forward_downstream(
        &self,
        stream_id: &str,
        frame: SignedEnvelope,
    ) -> Result<(), RelayError> {
        let inner = self.inner.lock().unwrap();
        let route = inner.streams.get(stream_id).ok_or(RelayError::NotFound)?;
        route.to_app.send(frame).map_err(|_| RelayError::NotFound)
    }

    /// Streams belonging to a device, so revocation can close them.
    pub fn streams_for_device(&self, device_id: &str) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .streams
            .values()
            .filter(|route| route.device_id == device_id)
            .map(|route| route.stream_id.clone())
            .collect()
    }

    pub fn is_online(&self, runtime_id: &str) -> bool {
        self.inner.lock().unwrap().online.contains_key(runtime_id)
    }

    pub fn require_online(&self, runtime_id: &str) -> Result<RuntimeOnline, RelayError> {
        self.inner
            .lock()
            .unwrap()
            .online
            .get(runtime_id)
            .cloned()
            .ok_or(RelayError::RuntimeOffline)
    }

    /// Hosts this device may see: exactly the one it is paired to, and only
    /// while it is online.
    pub fn hosts_for_device(&self, device_id: &str) -> Vec<RuntimeOnline> {
        let inner = self.inner.lock().unwrap();
        let Some(device) = inner.devices.get(device_id) else {
            return Vec::new();
        };
        inner
            .online
            .get(&device.runtime_id)
            .cloned()
            .into_iter()
            .collect()
    }
}

fn mint_pair(
    inner: &mut Inner,
    device_id: &str,
    runtime_id: &str,
    scope: PairingScope,
    now: i64,
) -> (String, String) {
    let access = random_b64(32);
    let refresh = random_b64(32);
    inner.counter += 1;
    let jti = format!("jti_{}", inner.counter);

    let base = TokenClaims {
        iss: "leveler-relay".to_string(),
        sub: device_id.to_string(),
        aud: runtime_id.to_string(),
        jti: jti.clone(),
        iat: now,
        exp: now + ACCESS_TOKEN_TTL_SECS,
        scope: SCOPE_REMOTE_SESSION.to_string(),
        pairing_scope: scope,
        token_use: TokenUse::Access,
    };
    inner.access.insert(
        access.clone(),
        TokenRecord {
            claims: base.clone(),
        },
    );
    inner.refresh.insert(
        refresh.clone(),
        TokenRecord {
            claims: TokenClaims {
                exp: now + REFRESH_TOKEN_TTL_SECS,
                token_use: TokenUse::Refresh,
                ..base
            },
        },
    );
    (access, refresh)
}

/// CSPRNG bytes, base64url without padding.
fn random_b64(bytes: usize) -> String {
    use ring::rand::SecureRandom as _;
    let mut buffer = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut buffer)
        .expect("system RNG is available");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buffer)
}

/// Compare without an early exit, matching the helper the web and transport
/// layers already use. The values here are hashes rather than the secrets
/// themselves, but a byte-by-byte comparison would still leak how far a guess
/// got, so it accumulates the whole difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}
