//! Server-side helpers for Multi Round-Trip Requests (SEP-2322).
//!
//! The final protocol carries continuation state through an untrusted client.
//! [`RequestStateCodec`] produces versioned, expiring, HMAC-SHA256-protected
//! tokens so stateless server instances can share continuation state safely by
//! sharing the same key.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};

use crate::protocol::InputResponses;

const TOKEN_VERSION: &str = "v1";
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
    /// The configured TTL must allow the state to live for some amount of time.
    #[error("request-state TTL must be greater than zero")]
    ZeroTtl,
    /// The serialized state exceeded the configured token-size limit.
    #[error("request-state token exceeds the configured maximum of {0} bytes")]
    TooLarge(usize),
    /// The token did not have the versioned three-part wire shape.
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
/// continuation token.
#[derive(Clone)]
pub struct RequestStateCodec {
    key: std::sync::Arc<[u8]>,
    ttl: Duration,
    max_token_bytes: usize,
}

impl std::fmt::Debug for RequestStateCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestStateCodec")
            .field("key", &"<redacted>")
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
    pub fn new(key: impl AsRef<[u8]>, ttl: Duration) -> Result<Self, RequestStateError> {
        let key = key.as_ref();
        if key.len() < 32 {
            return Err(RequestStateError::WeakKey);
        }
        if ttl.is_zero() {
            return Err(RequestStateError::ZeroTtl);
        }
        Ok(Self {
            key: std::sync::Arc::from(key),
            ttl,
            max_token_bytes: DEFAULT_MAX_TOKEN_BYTES,
        })
    }

    /// Set the maximum accepted and emitted token size.
    pub fn with_max_token_bytes(mut self, max_token_bytes: usize) -> Self {
        self.max_token_bytes = max_token_bytes;
        self
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
        let signed = format!("{TOKEN_VERSION}.{payload}");
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
        let payload = parts.next().ok_or(RequestStateError::Malformed)?;
        let signature = parts.next().ok_or(RequestStateError::Malformed)?;
        if parts.next().is_some() {
            return Err(RequestStateError::Malformed);
        }
        if version != TOKEN_VERSION {
            return Err(RequestStateError::UnsupportedVersion);
        }

        let supplied_signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| RequestStateError::Malformed)?;
        let signed = format!("{version}.{payload}");
        let expected_signature = hmac_sha256(&self.key, signed.as_bytes());
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
}

fn unix_seconds() -> Result<u64, RequestStateError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| RequestStateError::Clock)
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
}
