//! Authenticated, scope-bound remote-to-local relay contracts.
//!
//! The relay deliberately has no socket listener. A [`RelayService`] is owned by
//! the private per-user `wta-node` daemon and is reached only through an
//! authenticated bridge/daemon channel. Its signing key, revocations, nonce
//! ledger and bounded event journal therefore survive a transport reconnect,
//! while a daemon restart fails closed and invalidates every issued capability.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const RELAY_TOKEN_VERSION: u16 = 1;
const DEFAULT_TTL_MS: u64 = 5 * 60 * 1_000;
const MAX_TTL_MS: u64 = 15 * 60 * 1_000;
const MAX_TEXT_BYTES: usize = 16 * 1_024;
const MAX_METADATA_BYTES: usize = 64 * 1_024;
const MAX_EVENTS: usize = 2_048;
const MAX_LIST_LIMIT: usize = 200;
const MAX_NONCES_PER_TOKEN: usize = 8_192;

pub const RELAY_RPC_METHODS: &[&str] = &[
    "relay.capability.issue",
    "relay.capability.revoke",
    "relay.focus",
    "relay.list",
    "relay.notify",
    "relay.progress",
    "relay.status",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayScope {
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface_id: Option<String>,
}

impl RelayScope {
    fn validate(&self) -> Result<()> {
        validate_identifier("workspace_id", &self.workspace_id)?;
        if let Some(surface_id) = &self.surface_id {
            validate_identifier("surface_id", surface_id)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayOperation {
    Notify,
    Status,
    Progress,
    Focus,
    List,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayCapabilityClaims {
    pub version: u16,
    pub token_id: String,
    pub scope: RelayScope,
    pub operations: Vec<RelayOperation>,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueCapabilityRequest {
    pub scope: RelayScope,
    pub operations: Vec<RelayOperation>,
    #[serde(default = "default_ttl_ms")]
    pub ttl_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedCapability {
    pub token: String,
    pub claims: RelayCapabilityClaims,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeCapabilityRequest {
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAuthorization {
    pub token: String,
    pub nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayNotifyRequest {
    pub authorization: RelayAuthorization,
    pub scope: RelayScope,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayStatusRequest {
    pub authorization: RelayAuthorization,
    pub scope: RelayScope,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayProgressRequest {
    pub authorization: RelayAuthorization,
    pub scope: RelayScope,
    pub fraction: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayFocusRequest {
    pub authorization: RelayAuthorization,
    pub scope: RelayScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayListRequest {
    pub authorization: RelayAuthorization,
    pub scope: RelayScope,
    #[serde(default)]
    pub after_sequence: u64,
    #[serde(default = "default_list_limit")]
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayEventKind {
    Notify,
    Status,
    Progress,
    Focus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayEvent {
    pub event_id: String,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub scope: RelayScope,
    pub kind: RelayEventKind,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RelayListResponse {
    pub events: Vec<RelayEvent>,
    pub last_sequence: u64,
}

/// Daemon-local relay authority and event journal.
///
/// Keeping this value inside the private per-user daemon lets a reconnecting
/// SSH bridge resume the same journal without creating a network listener. A
/// daemon restart intentionally rotates the signing key.
pub struct RelayService {
    signing_key: [u8; 32],
    revoked_tokens: HashSet<String>,
    used_nonces: HashMap<String, HashMap<String, u64>>,
    events: VecDeque<RelayEvent>,
    next_sequence: u64,
}

impl Default for RelayService {
    fn default() -> Self {
        Self::new()
    }
}

impl RelayService {
    pub fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update(Uuid::new_v4().as_bytes());
        hasher.update(Uuid::new_v4().as_bytes());
        hasher.update(now_ms().to_le_bytes());
        Self {
            signing_key: hasher.finalize().into(),
            revoked_tokens: HashSet::new(),
            used_nonces: HashMap::new(),
            events: VecDeque::new(),
            next_sequence: 1,
        }
    }

    #[cfg(test)]
    fn with_key(signing_key: [u8; 32]) -> Self {
        Self {
            signing_key,
            revoked_tokens: HashSet::new(),
            used_nonces: HashMap::new(),
            events: VecDeque::new(),
            next_sequence: 1,
        }
    }

    pub fn dispatch(&mut self, method: &str, params: &Value) -> Result<Value> {
        self.dispatch_at(method, params, now_ms())
    }

    fn dispatch_at(&mut self, method: &str, params: &Value, now: u64) -> Result<Value> {
        match method {
            "relay.capability.issue" => {
                let request: IssueCapabilityRequest = decode(params, method)?;
                serde_json::to_value(self.issue_at(request, now)?)
                    .context("serialize issued relay capability")
            }
            "relay.capability.revoke" => {
                let request: RevokeCapabilityRequest = decode(params, method)?;
                let claims = self.decode_and_verify(&request.token)?;
                let revoked = self.revoked_tokens.insert(claims.token_id.clone());
                self.used_nonces.remove(&claims.token_id);
                Ok(json!({"revoked": revoked, "token_id": claims.token_id}))
            }
            "relay.notify" => {
                let request: RelayNotifyRequest = decode(params, method)?;
                self.authorize(
                    &request.authorization,
                    &request.scope,
                    RelayOperation::Notify,
                    now,
                )?;
                validate_text("title", &request.title, 1, 512)?;
                validate_text("body", &request.body, 0, MAX_TEXT_BYTES)?;
                if !request.level.is_empty() {
                    validate_text("level", &request.level, 1, 32)?;
                }
                validate_metadata(&request.metadata)?;
                self.record(
                    request.scope,
                    RelayEventKind::Notify,
                    json!({
                        "title": request.title,
                        "body": request.body,
                        "level": request.level,
                        "metadata": request.metadata,
                    }),
                    now,
                )
            }
            "relay.status" => {
                let request: RelayStatusRequest = decode(params, method)?;
                self.authorize(
                    &request.authorization,
                    &request.scope,
                    RelayOperation::Status,
                    now,
                )?;
                validate_text("state", &request.state, 1, 64)?;
                if let Some(detail) = &request.detail {
                    validate_text("detail", detail, 0, MAX_TEXT_BYTES)?;
                }
                validate_metadata(&request.metadata)?;
                self.record(
                    request.scope,
                    RelayEventKind::Status,
                    json!({
                        "state": request.state,
                        "detail": request.detail,
                        "metadata": request.metadata,
                    }),
                    now,
                )
            }
            "relay.progress" => {
                let request: RelayProgressRequest = decode(params, method)?;
                self.authorize(
                    &request.authorization,
                    &request.scope,
                    RelayOperation::Progress,
                    now,
                )?;
                if !request.fraction.is_finite() || !(0.0..=1.0).contains(&request.fraction) {
                    bail!("fraction must be finite and between 0.0 and 1.0");
                }
                if let Some(label) = &request.label {
                    validate_text("label", label, 0, 512)?;
                }
                validate_metadata(&request.metadata)?;
                self.record(
                    request.scope,
                    RelayEventKind::Progress,
                    json!({
                        "fraction": request.fraction,
                        "label": request.label,
                        "metadata": request.metadata,
                    }),
                    now,
                )
            }
            "relay.focus" => {
                let request: RelayFocusRequest = decode(params, method)?;
                self.authorize(
                    &request.authorization,
                    &request.scope,
                    RelayOperation::Focus,
                    now,
                )?;
                if let Some(reason) = &request.reason {
                    validate_text("reason", reason, 0, 512)?;
                }
                self.record(
                    request.scope,
                    RelayEventKind::Focus,
                    json!({"reason": request.reason}),
                    now,
                )
            }
            "relay.list" => {
                let request: RelayListRequest = decode(params, method)?;
                self.authorize(
                    &request.authorization,
                    &request.scope,
                    RelayOperation::List,
                    now,
                )?;
                if request.limit == 0 || request.limit > MAX_LIST_LIMIT {
                    bail!("limit must be between 1 and {MAX_LIST_LIMIT}");
                }
                let events = self
                    .events
                    .iter()
                    .filter(|event| event.sequence > request.after_sequence)
                    .filter(|event| scope_contains(&request.scope, &event.scope))
                    .take(request.limit)
                    .cloned()
                    .collect::<Vec<_>>();
                let last_sequence = events
                    .last()
                    .map(|event| event.sequence)
                    .unwrap_or(request.after_sequence);
                serde_json::to_value(RelayListResponse {
                    events,
                    last_sequence,
                })
                .context("serialize relay event list")
            }
            _ => bail!("unknown relay method: {method}"),
        }
    }

    fn issue_at(&self, request: IssueCapabilityRequest, now: u64) -> Result<IssuedCapability> {
        request.scope.validate()?;
        if request.ttl_ms == 0 || request.ttl_ms > MAX_TTL_MS {
            bail!("ttl_ms must be between 1 and {MAX_TTL_MS}");
        }
        let operations = request
            .operations
            .into_iter()
            .collect::<BTreeSet<RelayOperation>>();
        if operations.is_empty() {
            bail!("at least one relay operation is required");
        }
        let claims = RelayCapabilityClaims {
            version: RELAY_TOKEN_VERSION,
            token_id: Uuid::new_v4().to_string(),
            scope: request.scope,
            operations: operations.into_iter().collect(),
            issued_at_ms: now,
            expires_at_ms: now
                .checked_add(request.ttl_ms)
                .context("relay capability expiration overflow")?,
        };
        let payload = serde_json::to_vec(&claims).context("serialize relay capability claims")?;
        let signature = hmac_sha256(&self.signing_key, &payload);
        Ok(IssuedCapability {
            token: format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(payload),
                URL_SAFE_NO_PAD.encode(signature)
            ),
            claims,
        })
    }

    fn authorize(
        &mut self,
        authorization: &RelayAuthorization,
        requested_scope: &RelayScope,
        operation: RelayOperation,
        now: u64,
    ) -> Result<RelayCapabilityClaims> {
        validate_nonce(&authorization.nonce)?;
        requested_scope.validate()?;
        let claims = self.decode_and_verify(&authorization.token)?;
        if claims.version != RELAY_TOKEN_VERSION {
            bail!("unsupported relay capability version");
        }
        if now < claims.issued_at_ms || now >= claims.expires_at_ms {
            bail!("relay capability is expired or not yet valid");
        }
        if self.revoked_tokens.contains(&claims.token_id) {
            bail!("relay capability has been revoked");
        }
        if !claims.operations.contains(&operation) {
            bail!("relay capability does not permit {operation:?}");
        }
        if !scope_contains(&claims.scope, requested_scope) {
            bail!("requested relay scope is outside the capability scope");
        }

        self.prune_nonces(now);
        let nonces = self.used_nonces.entry(claims.token_id.clone()).or_default();
        if nonces.contains_key(&authorization.nonce) {
            bail!("relay nonce has already been used");
        }
        if nonces.len() >= MAX_NONCES_PER_TOKEN {
            bail!("relay capability nonce budget is exhausted");
        }
        nonces.insert(authorization.nonce.clone(), claims.expires_at_ms);
        Ok(claims)
    }

    fn decode_and_verify(&self, token: &str) -> Result<RelayCapabilityClaims> {
        let (payload, signature) = token
            .split_once('.')
            .context("malformed relay capability token")?;
        if signature.contains('.') {
            bail!("malformed relay capability token");
        }
        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .context("decode relay capability payload")?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .context("decode relay capability signature")?;
        let expected = hmac_sha256(&self.signing_key, &payload);
        if !constant_time_eq(&signature, &expected) {
            bail!("invalid relay capability signature");
        }
        let claims: RelayCapabilityClaims =
            serde_json::from_slice(&payload).context("decode relay capability claims")?;
        claims.scope.validate()?;
        validate_identifier("token_id", &claims.token_id)?;
        Ok(claims)
    }

    fn prune_nonces(&mut self, now: u64) {
        self.used_nonces.retain(|_, nonces| {
            nonces.retain(|_, expires_at_ms| *expires_at_ms > now);
            !nonces.is_empty()
        });
    }

    fn record(
        &mut self,
        scope: RelayScope,
        kind: RelayEventKind,
        payload: Value,
        now: u64,
    ) -> Result<Value> {
        let event = RelayEvent {
            event_id: Uuid::new_v4().to_string(),
            sequence: self.next_sequence,
            timestamp_ms: now,
            scope,
            kind,
            payload,
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.events.push_back(event.clone());
        if self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
        serde_json::to_value(event).context("serialize relay event")
    }
}

fn scope_contains(granted: &RelayScope, requested: &RelayScope) -> bool {
    granted.workspace_id == requested.workspace_id
        && match &granted.surface_id {
            Some(surface_id) => requested.surface_id.as_ref() == Some(surface_id),
            None => true,
        }
}

fn validate_identifier(name: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 {
        bail!("{name} must contain between 1 and 256 bytes");
    }
    if value.chars().any(char::is_control) {
        bail!("{name} must not contain control characters");
    }
    Ok(())
}

fn validate_nonce(nonce: &str) -> Result<()> {
    if !(8..=128).contains(&nonce.len())
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
    {
        bail!("nonce must be 8-128 ASCII unreserved characters");
    }
    Ok(())
}

fn validate_text(name: &str, value: &str, minimum: usize, maximum: usize) -> Result<()> {
    if value.len() < minimum || value.len() > maximum {
        bail!("{name} must contain between {minimum} and {maximum} bytes");
    }
    if value.contains('\0') {
        bail!("{name} must not contain NUL");
    }
    Ok(())
}

fn validate_metadata(metadata: &Value) -> Result<()> {
    if !metadata.is_null() && !metadata.is_object() {
        bail!("metadata must be an object or null");
    }
    if serde_json::to_vec(metadata)?.len() > MAX_METADATA_BYTES {
        bail!("metadata exceeds {MAX_METADATA_BYTES} bytes");
    }
    Ok(())
}

fn decode<T>(params: &Value, method: &str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_value(params.clone()).with_context(|| format!("invalid {method} params"))
}

fn default_ttl_ms() -> u64 {
    DEFAULT_TTL_MS
}

fn default_list_limit() -> usize {
    100
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK_SIZE: usize = 64;
    let mut normalized = [0_u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36_u8; BLOCK_SIZE];
    let mut outer_pad = [0x5c_u8; BLOCK_SIZE];
    for index in 0..BLOCK_SIZE {
        inner_pad[index] ^= normalized[index];
        outer_pad[index] ^= normalized[index];
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(data);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_hash);
    outer.finalize().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(
        service: &mut RelayService,
        scope: RelayScope,
        operations: Vec<RelayOperation>,
        now: u64,
    ) -> IssuedCapability {
        let params = serde_json::to_value(IssueCapabilityRequest {
            scope,
            operations,
            ttl_ms: 1_000,
        })
        .unwrap();
        serde_json::from_value(
            service
                .dispatch_at("relay.capability.issue", &params, now)
                .unwrap(),
        )
        .unwrap()
    }

    fn auth(token: &str, nonce: &str) -> RelayAuthorization {
        RelayAuthorization {
            token: token.to_string(),
            nonce: nonce.to_string(),
        }
    }

    fn surface(workspace: &str, surface: &str) -> RelayScope {
        RelayScope {
            workspace_id: workspace.to_string(),
            surface_id: Some(surface.to_string()),
        }
    }

    #[test]
    fn surface_capability_cannot_cross_surface_or_workspace() {
        let mut service = RelayService::with_key([7; 32]);
        let capability = issue(
            &mut service,
            surface("workspace-a", "surface-a"),
            vec![RelayOperation::Notify],
            10_000,
        );
        for (nonce, scope) in [
            ("nonce-0001", surface("workspace-a", "surface-b")),
            ("nonce-0002", surface("workspace-b", "surface-a")),
        ] {
            let params = serde_json::to_value(RelayNotifyRequest {
                authorization: auth(&capability.token, nonce),
                scope,
                title: "not allowed".into(),
                body: String::new(),
                level: "info".into(),
                metadata: Value::Null,
            })
            .unwrap();
            assert!(service
                .dispatch_at("relay.notify", &params, 10_100)
                .unwrap_err()
                .to_string()
                .contains("outside"));
        }
        assert!(service.events.is_empty());
    }

    #[test]
    fn expired_capability_is_rejected() {
        let mut service = RelayService::with_key([8; 32]);
        let capability = issue(
            &mut service,
            surface("workspace-a", "surface-a"),
            vec![RelayOperation::Status],
            20_000,
        );
        let params = serde_json::to_value(RelayStatusRequest {
            authorization: auth(&capability.token, "nonce-0003"),
            scope: surface("workspace-a", "surface-a"),
            state: "working".into(),
            detail: None,
            metadata: Value::Null,
        })
        .unwrap();
        assert!(service
            .dispatch_at("relay.status", &params, 21_000)
            .unwrap_err()
            .to_string()
            .contains("expired"));
    }

    #[test]
    fn nonce_is_single_use_even_for_same_valid_request() {
        let mut service = RelayService::with_key([9; 32]);
        let capability = issue(
            &mut service,
            surface("workspace-a", "surface-a"),
            vec![RelayOperation::Progress],
            30_000,
        );
        let params = serde_json::to_value(RelayProgressRequest {
            authorization: auth(&capability.token, "nonce-0004"),
            scope: surface("workspace-a", "surface-a"),
            fraction: 0.5,
            label: Some("building".into()),
            metadata: Value::Null,
        })
        .unwrap();
        service
            .dispatch_at("relay.progress", &params, 30_100)
            .unwrap();
        assert!(service
            .dispatch_at("relay.progress", &params, 30_200)
            .unwrap_err()
            .to_string()
            .contains("already been used"));
        assert_eq!(service.events.len(), 1);
    }

    #[test]
    fn revoked_capability_fails_closed() {
        let mut service = RelayService::with_key([10; 32]);
        let capability = issue(
            &mut service,
            surface("workspace-a", "surface-a"),
            vec![RelayOperation::Focus],
            40_000,
        );
        let revoke = serde_json::to_value(RevokeCapabilityRequest {
            token: capability.token.clone(),
        })
        .unwrap();
        service
            .dispatch_at("relay.capability.revoke", &revoke, 40_050)
            .unwrap();
        let params = serde_json::to_value(RelayFocusRequest {
            authorization: auth(&capability.token, "nonce-0005"),
            scope: surface("workspace-a", "surface-a"),
            reason: Some("agent needs attention".into()),
        })
        .unwrap();
        assert!(service
            .dispatch_at("relay.focus", &params, 40_100)
            .unwrap_err()
            .to_string()
            .contains("revoked"));
    }

    #[test]
    fn operation_allowlist_and_tamper_are_enforced() {
        let mut service = RelayService::with_key([11; 32]);
        let capability = issue(
            &mut service,
            surface("workspace-a", "surface-a"),
            vec![RelayOperation::List],
            50_000,
        );
        let notify = serde_json::to_value(RelayNotifyRequest {
            authorization: auth(&capability.token, "nonce-0006"),
            scope: surface("workspace-a", "surface-a"),
            title: "forbidden".into(),
            body: String::new(),
            level: String::new(),
            metadata: Value::Null,
        })
        .unwrap();
        assert!(service
            .dispatch_at("relay.notify", &notify, 50_100)
            .unwrap_err()
            .to_string()
            .contains("does not permit"));

        let mut tampered = capability.token.clone().into_bytes();
        let index = tampered.len() / 3;
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
        let list = serde_json::to_value(RelayListRequest {
            authorization: auth(&String::from_utf8(tampered).unwrap(), "nonce-0007"),
            scope: surface("workspace-a", "surface-a"),
            after_sequence: 0,
            limit: 100,
        })
        .unwrap();
        assert!(service.dispatch_at("relay.list", &list, 50_100).is_err());
    }

    #[test]
    fn workspace_list_sees_only_its_workspace_events() {
        let mut service = RelayService::with_key([12; 32]);
        let workspace_scope = RelayScope {
            workspace_id: "workspace-a".into(),
            surface_id: None,
        };
        let capability = issue(
            &mut service,
            workspace_scope.clone(),
            vec![RelayOperation::Notify, RelayOperation::List],
            60_000,
        );
        for (index, scope) in [
            surface("workspace-a", "surface-a"),
            surface("workspace-a", "surface-b"),
        ]
        .into_iter()
        .enumerate()
        {
            let params = serde_json::to_value(RelayNotifyRequest {
                authorization: auth(&capability.token, &format!("nonce-10{index:02}")),
                scope,
                title: format!("notification {index}"),
                body: String::new(),
                level: "info".into(),
                metadata: Value::Null,
            })
            .unwrap();
            service
                .dispatch_at("relay.notify", &params, 60_100 + index as u64)
                .unwrap();
        }
        let list = serde_json::to_value(RelayListRequest {
            authorization: auth(&capability.token, "nonce-list-1"),
            scope: workspace_scope,
            after_sequence: 0,
            limit: 100,
        })
        .unwrap();
        let response: RelayListResponse =
            serde_json::from_value(service.dispatch_at("relay.list", &list, 60_200).unwrap())
                .unwrap();
        assert_eq!(response.events.len(), 2);
        assert!(response
            .events
            .iter()
            .all(|event| event.scope.workspace_id == "workspace-a"));
    }

    #[test]
    fn hmac_matches_rfc_4231_sha256_vector() {
        let key = [0x0b; 20];
        assert_eq!(
            hex::encode(hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn capability_from_another_bridge_is_rejected() {
        let mut first_bridge = RelayService::with_key([13; 32]);
        let capability = issue(
            &mut first_bridge,
            surface("workspace-a", "surface-a"),
            vec![RelayOperation::List],
            70_000,
        );
        let mut second_bridge = RelayService::with_key([14; 32]);
        let list = serde_json::to_value(RelayListRequest {
            authorization: auth(&capability.token, "nonce-list-2"),
            scope: surface("workspace-a", "surface-a"),
            after_sequence: 0,
            limit: 100,
        })
        .unwrap();
        assert!(second_bridge
            .dispatch_at("relay.list", &list, 70_100)
            .unwrap_err()
            .to_string()
            .contains("signature"));
    }
}
