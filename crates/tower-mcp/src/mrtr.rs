//! Server-side helpers for Multi Round-Trip Requests (SEP-2322).
//!
//! The final protocol carries continuation state through an untrusted client.
//! [`RequestStateCodec`] produces versioned, expiring, HMAC-SHA256-protected
//! tokens so stateless server instances can share continuation state safely by
//! sharing the same key. Every signing key carries a key id, which lets a
//! codec hold one active signing key plus a set of retired verification keys
//! so a key can be rotated without invalidating tokens already in flight; see
//! [`RequestStateCodec::with_key_id`] and
//! [`RequestStateCodec::with_verification_key`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::protocol::InputResponses;

const TOKEN_VERSION_V1: &str = "v1";
const TOKEN_VERSION_V2: &str = "v2";
const SHA256_BLOCK_SIZE: usize = 64;
const DEFAULT_MAX_TOKEN_BYTES: usize = 64 * 1024;

/// MRTR continuation values supplied on a retry of the original request.
///
/// The router inserts this value into [`crate::RequestContext`] for
/// `tools/call`, `prompts/get`, and `resources/read`. Handlers can use
/// [`RequestContext::mrtr`](crate::RequestContext::mrtr) or the convenience
/// accessors on the context.
#[derive(Debug, Clone, Default)]
pub struct MrtrRequest {
    input_responses: Option<InputResponses>,
    request_state: Option<String>,
}

impl MrtrRequest {
    pub(crate) fn new(
        input_responses: Option<InputResponses>,
        request_state: Option<String>,
    ) -> Self {
        Self {
            input_responses,
            request_state,
        }
    }

    /// Client responses keyed by the identifiers from the prior
    /// `inputRequests` map.
    pub fn input_responses(&self) -> Option<&InputResponses> {
        self.input_responses.as_ref()
    }

    /// Opaque continuation token echoed by the client.
    pub fn request_state(&self) -> Option<&str> {
        self.request_state.as_deref()
    }

    /// Consume the continuation values.
    pub fn into_parts(self) -> (Option<InputResponses>, Option<String>) {
        (self.input_responses, self.request_state)
    }
}

/// Errors produced while encoding or validating opaque MRTR request state.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RequestStateError {
    /// HMAC keys shorter than 256 bits do not meet this codec's minimum.
    #[error("request-state key must be at least 32 bytes")]
    WeakKey,
    /// A key id was empty or used characters outside the restricted key-id
    /// charset (ASCII alphanumeric, `-`, `_`); the charset deliberately
    /// excludes `.`, the token field separator.
    #[error(
        "request-state key id must be non-empty and contain only ASCII alphanumeric, '-', or '_' characters"
    )]
    InvalidKeyId,
    /// The configured TTL must allow the state to live for some amount of time.
    #[error("request-state TTL must be greater than zero")]
    ZeroTtl,
    /// The serialized state exceeded the configured token-size limit.
    #[error("request-state token exceeds the configured maximum of {0} bytes")]
    TooLarge(usize),
    /// The token did not have the expected wire shape for its version.
    #[error("request-state token is malformed")]
    Malformed,
    /// The token uses a codec version this server does not understand.
    #[error("unsupported request-state token version")]
    UnsupportedVersion,
    /// The HMAC did not match the payload.
    #[error("request-state integrity verification failed")]
    Integrity,
    /// The token is no longer valid.
    #[error("request-state token has expired")]
    Expired,
    /// A token bound to one authorization subject was used by another.
    #[error("request-state token is not bound to the current subject")]
    SubjectMismatch,
    /// The state value could not be serialized.
    #[error("failed to serialize request state: {0}")]
    Encode(#[source] serde_json::Error),
    /// The state value could not be decoded as the expected type.
    #[error("failed to decode request state: {0}")]
    Decode(#[source] serde_json::Error),
    /// The system clock is earlier than the Unix epoch.
    #[error("system clock is earlier than the Unix epoch")]
    Clock,
}

#[derive(Debug, Serialize, serde::Deserialize)]
struct StateEnvelope<T> {
    issued_at: u64,
    expires_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    state: T,
}

/// HMAC-SHA256 codec for opaque, expiring MRTR `requestState` values.
///
/// Construct the codec with the same key and TTL on every server instance
/// that may receive a retry. Use [`encode_for`](Self::encode_for) and
/// [`decode_for`](Self::decode_for) when an authenticated subject is
/// available; subject binding prevents one user from replaying another user's
/// continuation token. Binding is intentionally explicit: authentication
/// middleware may place any application-defined principal type in request
/// extensions, so the transport-neutral codec cannot safely infer one.
///
/// ## Key rotation
///
/// Every token carries the key id of the key that signed it. A codec holds
/// exactly one signing key, used for every new token, plus any number of
/// retired verification keys, tried only on decode. To rotate a compromised
/// or aging key without invalidating tokens already in flight:
///
/// 1. Deploy a codec whose signing key is the new key (via [`new`](Self::new)
///    or [`with_key_id`](Self::with_key_id) if you want a chosen id rather
///    than the derived default).
/// 2. Register the old key as a verification key with
///    [`with_verification_key`](Self::with_verification_key), keyed by its
///    old key id, so tokens issued before the rotation still decode.
/// 3. Once the configured TTL has fully elapsed since the rotation (so no
///    token signed with the old key can still be unexpired), drop the call
///    to `with_verification_key` for that key and redeploy; the old key is
///    now fully retired.
///
/// [`new`](Self::new) without [`with_key_id`](Self::with_key_id) derives a
/// stable key id from the key bytes themselves, so independent instances
/// constructed from the same key without an explicit id still agree on the
/// id and interoperate.
#[derive(Clone)]
pub struct RequestStateCodec {
    key: Arc<[u8]>,
    key_id: Arc<str>,
    verification_keys: Arc<HashMap<String, Arc<[u8]>>>,
    ttl: Duration,
    max_token_bytes: usize,
}

impl std::fmt::Debug for RequestStateCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestStateCodec")
            .field("key", &"<redacted>")
            .field("key_id", &self.key_id)
            .field(
                "verification_key_ids",
                &self.verification_keys.keys().collect::<Vec<_>>(),
            )
            .field("ttl", &self.ttl)
            .field("max_token_bytes", &self.max_token_bytes)
            .finish()
    }
}

impl RequestStateCodec {
    /// Create a codec from a shared key and token TTL.
    ///
    /// The key must contain at least 32 bytes of entropy. Configuration
    /// secrets should be decoded to raw bytes before calling this constructor.
    ///
    /// The signing key id defaults to a value derived deterministically from
    /// the key bytes, so independent instances constructed from the same key
    /// agree on the id without further configuration. Call
    /// [`with_key_id`](Self::with_key_id) to use a chosen id instead, for
    /// example one that identifies the key in an external secret store.
    pub fn new(key: impl AsRef<[u8]>, ttl: Duration) -> Result<Self, RequestStateError> {
        let key = key.as_ref();
        if key.len() < 32 {
            return Err(RequestStateError::WeakKey);
        }
        if ttl.is_zero() {
            return Err(RequestStateError::ZeroTtl);
        }
        Ok(Self {
            key_id: derive_key_id(key),
            key: Arc::from(key),
            verification_keys: Arc::new(HashMap::new()),
            ttl,
            max_token_bytes: DEFAULT_MAX_TOKEN_BYTES,
        })
    }

    /// Set the maximum accepted and emitted token size.
    pub fn with_max_token_bytes(mut self, max_token_bytes: usize) -> Self {
        self.max_token_bytes = max_token_bytes;
        self
    }

    /// Set the key id embedded in every token this codec signs.
    ///
    /// The id must be non-empty and use only ASCII alphanumeric characters,
    /// `-`, or `_`; the token wire format uses `.` as a field separator, so
    /// key ids cannot contain it. Choosing an explicit, human-meaningful id
    /// (rather than relying on the key-derived default) makes rotation
    /// bookkeeping easier when several keys are in play.
    pub fn with_key_id(mut self, key_id: impl Into<String>) -> Result<Self, RequestStateError> {
        self.key_id = Arc::from(validate_key_id(key_id.into())?);
        Ok(self)
    }

    /// Register a retired key that this codec will still accept on decode.
    ///
    /// Verification keys are tried by key id only; they are never used to
    /// sign new tokens. Register the previous signing key here immediately
    /// after rotating to a new one, keyed by the id it used to sign, and keep
    /// it registered until the configured TTL has fully elapsed since the
    /// rotation. The key id rules and minimum key length are the same as for
    /// the signing key.
    pub fn with_verification_key(
        mut self,
        key_id: impl Into<String>,
        key: impl AsRef<[u8]>,
    ) -> Result<Self, RequestStateError> {
        let key_id = validate_key_id(key_id.into())?;
        let key = key.as_ref();
        if key.len() < 32 {
            return Err(RequestStateError::WeakKey);
        }
        Arc::make_mut(&mut self.verification_keys).insert(key_id, Arc::from(key));
        Ok(self)
    }

    /// Encode state without authorization-subject binding.
    pub fn encode<T: Serialize>(&self, state: &T) -> Result<String, RequestStateError> {
        self.encode_at(None, state, unix_seconds()?)
    }

    /// Encode state bound to an authenticated subject identifier.
    pub fn encode_for<T: Serialize>(
        &self,
        subject: impl Into<String>,
        state: &T,
    ) -> Result<String, RequestStateError> {
        self.encode_at(Some(subject.into()), state, unix_seconds()?)
    }

    /// Verify and decode state that was not subject-bound.
    pub fn decode<T: DeserializeOwned>(&self, token: &str) -> Result<T, RequestStateError> {
        self.decode_at(token, None, unix_seconds()?)
    }

    /// Verify and decode state for the current authenticated subject.
    pub fn decode_for<T: DeserializeOwned>(
        &self,
        token: &str,
        subject: &str,
    ) -> Result<T, RequestStateError> {
        self.decode_at(token, Some(subject), unix_seconds()?)
    }

    fn encode_at<T: Serialize>(
        &self,
        subject: Option<String>,
        state: &T,
        now: u64,
    ) -> Result<String, RequestStateError> {
        let ttl = self.ttl.as_secs();
        let envelope = StateEnvelope {
            issued_at: now,
            expires_at: now.saturating_add(ttl),
            subject,
            state,
        };
        let payload = serde_json::to_vec(&envelope).map_err(RequestStateError::Encode)?;
        let payload = URL_SAFE_NO_PAD.encode(payload);
        let signed = format!("{TOKEN_VERSION_V2}.{}.{payload}", self.key_id);
        let signature = URL_SAFE_NO_PAD.encode(hmac_sha256(&self.key, signed.as_bytes()));
        let token = format!("{signed}.{signature}");
        if token.len() > self.max_token_bytes {
            return Err(RequestStateError::TooLarge(self.max_token_bytes));
        }
        Ok(token)
    }

    fn decode_at<T: DeserializeOwned>(
        &self,
        token: &str,
        subject: Option<&str>,
        now: u64,
    ) -> Result<T, RequestStateError> {
        if token.len() > self.max_token_bytes {
            return Err(RequestStateError::TooLarge(self.max_token_bytes));
        }
        let mut parts = token.split('.');
        let version = parts.next().ok_or(RequestStateError::Malformed)?;

        // v1 tokens carry no key id and were always signed and verified with
        // the single configured key; keep accepting them against the current
        // signing key so upgrading a deployment does not break exchanges
        // already in flight. v2 tokens add the key id as a signed field, so
        // relabeling a token to a different key id or version invalidates
        // its signature.
        let (key, signed, payload, signature) = match version {
            TOKEN_VERSION_V1 => {
                let payload = parts.next().ok_or(RequestStateError::Malformed)?;
                let signature = parts.next().ok_or(RequestStateError::Malformed)?;
                if parts.next().is_some() {
                    return Err(RequestStateError::Malformed);
                }
                (
                    self.key.as_ref(),
                    format!("{version}.{payload}"),
                    payload,
                    signature,
                )
            }
            TOKEN_VERSION_V2 => {
                let key_id = parts.next().ok_or(RequestStateError::Malformed)?;
                let payload = parts.next().ok_or(RequestStateError::Malformed)?;
                let signature = parts.next().ok_or(RequestStateError::Malformed)?;
                if parts.next().is_some() {
                    return Err(RequestStateError::Malformed);
                }
                let key = self.key_for(key_id).ok_or(RequestStateError::Integrity)?;
                (
                    key,
                    format!("{version}.{key_id}.{payload}"),
                    payload,
                    signature,
                )
            }
            _ => return Err(RequestStateError::UnsupportedVersion),
        };

        let supplied_signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| RequestStateError::Malformed)?;
        let expected_signature = hmac_sha256(key, signed.as_bytes());
        if !constant_time_eq(&supplied_signature, &expected_signature) {
            return Err(RequestStateError::Integrity);
        }

        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| RequestStateError::Malformed)?;
        let envelope: StateEnvelope<T> =
            serde_json::from_slice(&payload).map_err(RequestStateError::Decode)?;
        if now > envelope.expires_at {
            return Err(RequestStateError::Expired);
        }
        match (envelope.subject.as_deref(), subject) {
            (None, None) => {}
            (Some(expected), Some(actual)) if expected == actual => {}
            _ => return Err(RequestStateError::SubjectMismatch),
        }
        Ok(envelope.state)
    }

    /// Resolve a token's key id to the key that should verify it: the
    /// current signing key, or a registered verification key.
    fn key_for(&self, key_id: &str) -> Option<&[u8]> {
        if key_id == self.key_id.as_ref() {
            return Some(&self.key);
        }
        self.verification_keys.get(key_id).map(|key| key.as_ref())
    }
}

fn unix_seconds() -> Result<u64, RequestStateError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| RequestStateError::Clock)
}

/// Derive the default signing key id for a codec that has not called
/// [`RequestStateCodec::with_key_id`]. The id is a stable function of the key
/// bytes, so instances built from the same key without an explicit id still
/// agree on which id signed a token.
fn derive_key_id(key: &[u8]) -> Arc<str> {
    let digest = Sha256::digest(key);
    let mut id = String::with_capacity(16);
    for byte in &digest[..8] {
        use std::fmt::Write;
        let _ = write!(id, "{byte:02x}");
    }
    Arc::from(id)
}

/// Validate a caller-supplied key id: non-empty, and restricted to
/// characters that cannot collide with the `.` token field separator.
fn validate_key_id(key_id: String) -> Result<String, RequestStateError> {
    if key_id.is_empty()
        || !key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(RequestStateError::InvalidKeyId);
    }
    Ok(key_id)
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut normalized = [0u8; SHA256_BLOCK_SIZE];
    if key.len() > SHA256_BLOCK_SIZE {
        normalized[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        normalized[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; SHA256_BLOCK_SIZE];
    let mut outer_pad = [0x5cu8; SHA256_BLOCK_SIZE];
    for ((inner, outer), key_byte) in inner_pad
        .iter_mut()
        .zip(outer_pad.iter_mut())
        .zip(normalized)
    {
        *inner ^= key_byte;
        *outer ^= key_byte;
    }

    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner);
    outer.finalize().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8; 32] = b"0123456789abcdef0123456789abcdef";

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct State {
        round: u8,
        value: String,
    }

    #[test]
    fn round_trips_shared_state() {
        let first = RequestStateCodec::new(KEY, Duration::from_secs(60)).unwrap();
        let second = RequestStateCodec::new(KEY, Duration::from_secs(60)).unwrap();
        let state = State {
            round: 2,
            value: "kept".into(),
        };
        let token = first.encode_at(None, &state, 100).unwrap();
        assert_eq!(second.decode_at::<State>(&token, None, 120).unwrap(), state);
    }

    #[test]
    fn rejects_tampering_expiry_and_wrong_subject() {
        let codec = RequestStateCodec::new(KEY, Duration::from_secs(10)).unwrap();
        let token = codec
            .encode_at(
                Some("alice".into()),
                &State {
                    round: 1,
                    value: "x".into(),
                },
                100,
            )
            .unwrap();

        assert!(matches!(
            codec.decode_at::<State>(&format!("{token}x"), Some("alice"), 101),
            Err(RequestStateError::Integrity | RequestStateError::Malformed)
        ));
        assert!(matches!(
            codec.decode_at::<State>(&token, Some("bob"), 101),
            Err(RequestStateError::SubjectMismatch)
        ));
        assert!(matches!(
            codec.decode_at::<State>(&token, Some("alice"), 111),
            Err(RequestStateError::Expired)
        ));
    }

    #[test]
    fn enforces_key_ttl_and_size_limits() {
        assert!(matches!(
            RequestStateCodec::new(b"short", Duration::from_secs(1)),
            Err(RequestStateError::WeakKey)
        ));
        assert!(matches!(
            RequestStateCodec::new(KEY, Duration::ZERO),
            Err(RequestStateError::ZeroTtl)
        ));
        let codec = RequestStateCodec::new(KEY, Duration::from_secs(1))
            .unwrap()
            .with_max_token_bytes(8);
        assert!(matches!(
            codec.encode(&"too large"),
            Err(RequestStateError::TooLarge(8))
        ));
    }

    const KEY_A: &[u8; 32] = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const KEY_B: &[u8; 32] = b"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    /// Build a token exactly as the pre-rotation codec did: `v1.<payload>.<sig>`,
    /// with no key id, signed against a single key.
    fn legacy_v1_token<T: Serialize>(key: &[u8], state: &T, now: u64, ttl_secs: u64) -> String {
        let envelope = StateEnvelope {
            issued_at: now,
            expires_at: now.saturating_add(ttl_secs),
            subject: None,
            state,
        };
        let payload = serde_json::to_vec(&envelope).unwrap();
        let payload = URL_SAFE_NO_PAD.encode(payload);
        let signed = format!("{TOKEN_VERSION_V1}.{payload}");
        let signature = URL_SAFE_NO_PAD.encode(hmac_sha256(key, signed.as_bytes()));
        format!("{signed}.{signature}")
    }

    #[test]
    fn rotates_signing_key_while_verifying_old_tokens() {
        let codec_a = RequestStateCodec::new(KEY_A, Duration::from_secs(60))
            .unwrap()
            .with_key_id("a")
            .unwrap();
        // New signing key "b", plus retired key "a" kept around for
        // verification during the rotation window.
        let codec_b = RequestStateCodec::new(KEY_B, Duration::from_secs(60))
            .unwrap()
            .with_key_id("b")
            .unwrap()
            .with_verification_key("a", KEY_A)
            .unwrap();

        let state = State {
            round: 1,
            value: "rotate".into(),
        };
        let token = codec_a.encode_at(None, &state, 100).unwrap();
        assert_eq!(
            codec_b.decode_at::<State>(&token, None, 120).unwrap(),
            state
        );
    }

    #[test]
    fn rejects_unknown_key_id() {
        let codec_a = RequestStateCodec::new(KEY_A, Duration::from_secs(60))
            .unwrap()
            .with_key_id("a")
            .unwrap();
        // "b" never registers "a" as a verification key.
        let codec_b = RequestStateCodec::new(KEY_B, Duration::from_secs(60))
            .unwrap()
            .with_key_id("b")
            .unwrap();

        let state = State {
            round: 1,
            value: "unknown".into(),
        };
        let token = codec_a.encode_at(None, &state, 100).unwrap();
        assert!(matches!(
            codec_b.decode_at::<State>(&token, None, 120),
            Err(RequestStateError::Integrity)
        ));
    }

    #[test]
    fn rejects_token_signed_by_retired_key() {
        let state = State {
            round: 1,
            value: "retire".into(),
        };
        let token = RequestStateCodec::new(KEY_A, Duration::from_secs(60))
            .unwrap()
            .with_key_id("a")
            .unwrap()
            .encode_at(None, &state, 100)
            .unwrap();

        // While "a" is still registered for verification, the token decodes.
        let codec_with_a = RequestStateCodec::new(KEY_B, Duration::from_secs(60))
            .unwrap()
            .with_key_id("b")
            .unwrap()
            .with_verification_key("a", KEY_A)
            .unwrap();
        assert_eq!(
            codec_with_a.decode_at::<State>(&token, None, 120).unwrap(),
            state
        );

        // Once "a" is retired (no longer registered), the same token is
        // rejected instead of silently trusting a dropped key.
        let codec_without_a = RequestStateCodec::new(KEY_B, Duration::from_secs(60))
            .unwrap()
            .with_key_id("b")
            .unwrap();
        assert!(matches!(
            codec_without_a.decode_at::<State>(&token, None, 120),
            Err(RequestStateError::Integrity)
        ));
    }

    #[test]
    fn decodes_legacy_v1_tokens_with_the_signing_key() {
        let codec = RequestStateCodec::new(KEY, Duration::from_secs(60)).unwrap();
        let state = State {
            round: 3,
            value: "legacy".into(),
        };
        let token = legacy_v1_token(KEY, &state, 100, 60);
        assert_eq!(codec.decode_at::<State>(&token, None, 120).unwrap(), state);
    }

    #[test]
    fn rejects_altered_key_id_or_version_label() {
        let codec = RequestStateCodec::new(KEY_A, Duration::from_secs(60))
            .unwrap()
            .with_key_id("a")
            .unwrap();
        let state = State {
            round: 1,
            value: "label".into(),
        };
        let token = codec.encode_at(None, &state, 100).unwrap();
        let parts: Vec<&str> = token.splitn(4, '.').collect();
        assert_eq!(parts[0], "v2");
        assert_eq!(parts[1], "a");
        let (payload, signature) = (parts[2], parts[3]);

        // Relabeling to a different key id this codec also trusts (same
        // underlying key, different id) must still fail: the signature
        // covers the key id, not just the payload.
        let codec_with_alias = codec.clone().with_verification_key("alias", KEY_A).unwrap();
        let relabeled_key_id = format!("v2.alias.{payload}.{signature}");
        assert!(matches!(
            codec_with_alias.decode_at::<State>(&relabeled_key_id, None, 120),
            Err(RequestStateError::Integrity)
        ));

        // Relabeling the version to v1 must also fail: the signature covers
        // the version label too.
        let relabeled_version = format!("v1.{payload}.{signature}");
        assert!(matches!(
            codec.decode_at::<State>(&relabeled_version, None, 120),
            Err(RequestStateError::Integrity)
        ));
    }
}
