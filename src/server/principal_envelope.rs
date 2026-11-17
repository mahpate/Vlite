//! Signed principal envelope (feature-aca-ha ITEM-009, SEC-002/005, OD-009).
//!
//! A gateway authenticates the caller against the shared control state and
//! forwards the resulting scoped principal to a backend in one header:
//!
//! `x-vlite-principal: vle1.<kid>.<base64url(claims JSON)>.<base64url(Ed25519 signature)>`
//!
//! The signature covers `vle1.<kid>.<claims>`. The claims bind an audience,
//! an issue and expiry time, the control-state generation `A` the gateway
//! decided with, and the principal itself (no secret). A backend in
//! gateway-trusted mode verifies all of them and holds the highest `A` it has
//! accepted or been told, refusing anything older (RD-004).
//!
//! Keys: only the active signing key signs; a verifier holds at most two
//! public keys by explicit key identity (the old and new key during a
//! rotation overlap, FM-010) and refuses every other key identity. Key
//! material comes from the platform secret store (OD-009) and is never
//! written into control-state objects, envelopes, errors or logs. Skew,
//! lifetime and the minimum generation are injected, never defaulted
//! (OD-005, NFR-001).
// The signing half is used by the gateway (EPIC-004); the backend verifies.
#![cfg_attr(not(test), allow(dead_code))]
use super::*;
use base64::prelude::BASE64_URL_SAFE_NO_PAD;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::Serialize;
use std::sync::atomic::AtomicU64;

/// The request header a gateway sets and a trusted backend reads.
pub(super) const HEADER: &str = "x-vlite-principal";
const VERSION: &str = "vle1";
/// Bound on the header value before any decoding.
const MAX_TOKEN_BYTES: usize = 64 * 1024;

/// What the gateway asserts. `iat`, `exp` are Unix milliseconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Claims {
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
    /// Control-state generation `A` the gateway authorized with.
    pub generation: u64,
    pub principal: credentials::Principal,
}

/// Why an envelope was refused. Every refusal is a 401; the variant is only
/// used by tests and the error code, never echoes the envelope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    Malformed,
    UnknownKey,
    Signature,
    Audience,
    Tenant,
    NotYetValid,
    Expired,
    Lifetime,
    CredentialExpired,
    Stale,
    Principal,
}

impl Refusal {
    pub(super) fn error(self) -> ApiError {
        let (code, message) = match self {
            Self::Expired | Self::NotYetValid => (
                "envelope_expired",
                "principal envelope is outside its validity window",
            ),
            Self::CredentialExpired => (
                "credential_expired",
                "credential expired; request a new one",
            ),
            Self::Stale => (
                "envelope_stale",
                "principal envelope was issued from an older control-state generation",
            ),
            _ => ("envelope_invalid", "valid principal envelope required"),
        };
        ApiError::new(StatusCode::UNAUTHORIZED, code, message)
    }
}

fn valid_kid(kid: &str) -> bool {
    (1..=64).contains(&kid.len())
        && kid
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// The active signing key. Has no `Debug` and never exposes its seed.
pub(super) struct Signer {
    kid: String,
    pair: Ed25519KeyPair,
}

impl Signer {
    pub(super) fn from_seed(kid: &str, seed: &[u8; 32]) -> Result<Self, &'static str> {
        if !valid_kid(kid) {
            return Err("invalid key identity");
        }
        let pair = Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| "invalid signing key")?;
        Ok(Self {
            kid: kid.to_owned(),
            pair,
        })
    }

    pub(super) fn public_key(&self) -> [u8; 32] {
        let mut key = [0u8; 32];
        key.copy_from_slice(self.pair.public_key().as_ref());
        key
    }

    pub(super) fn sign(&self, claims: &Claims) -> Result<String, &'static str> {
        let json = serde_json::to_vec(claims).map_err(|_| "claims are not serializable")?;
        let signed = format!(
            "{VERSION}.{}.{}",
            self.kid,
            BASE64_URL_SAFE_NO_PAD.encode(json)
        );
        let signature = self.pair.sign(signed.as_bytes());
        let token = format!(
            "{signed}.{}",
            BASE64_URL_SAFE_NO_PAD.encode(signature.as_ref())
        );
        if token.len() > MAX_TOKEN_BYTES {
            return Err("principal envelope too large");
        }
        Ok(token)
    }
}

/// What a backend requires of every envelope.
pub(super) struct Policy {
    /// This backend's audience; equality only.
    pub audience: String,
    /// The tenant this backend serves (`None`: an untenanted deployment).
    pub tenant: Option<String>,
    /// Tolerated clock difference, milliseconds (FM-008).
    pub skew_ms: u64,
    /// Longest accepted `exp - iat`, milliseconds.
    pub max_lifetime_ms: u64,
    /// Lowest generation `A` accepted at start.
    pub minimum_generation: u64,
}

pub(super) struct Verifier {
    keys: Vec<(String, [u8; 32])>,
    policy: Policy,
    /// Highest `A` accepted or announced; never decreases.
    floor: AtomicU64,
}

impl Verifier {
    /// One key, or two during a rotation overlap, with distinct identities.
    pub(super) fn new(keys: Vec<(String, [u8; 32])>, policy: Policy) -> Result<Self, &'static str> {
        if !(1..=2).contains(&keys.len()) {
            return Err("a verifier needs one key, or two during rotation");
        }
        if keys.iter().any(|(kid, _)| !valid_kid(kid)) {
            return Err("invalid key identity");
        }
        if keys.len() == 2 && keys[0].0 == keys[1].0 {
            return Err("duplicate key identity");
        }
        Ok(Self {
            keys,
            policy,
            floor: AtomicU64::new(0),
        })
    }

    /// Raises the minimum accepted generation (told by control state).
    pub(super) fn raise_minimum(&self, generation: u64) {
        self.floor.fetch_max(generation, Ordering::SeqCst);
    }

    pub(super) fn minimum(&self) -> u64 {
        self.floor
            .load(Ordering::SeqCst)
            .max(self.policy.minimum_generation)
    }

    /// Verifies `token` at `now` (Unix ms) and returns the scoped grant.
    pub(super) fn verify(&self, token: &str, now: u64) -> Result<Arc<credentials::Grant>, Refusal> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(Refusal::Malformed);
        }
        let mut parts = token.split('.');
        let (Some(VERSION), Some(kid), Some(payload), Some(signature), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(Refusal::Malformed);
        };
        let key = self
            .keys
            .iter()
            .find(|(known, _)| known == kid)
            .map(|(_, key)| key)
            .ok_or(Refusal::UnknownKey)?;
        let signature = BASE64_URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| Refusal::Malformed)?;
        let signed = &token[..VERSION.len() + kid.len() + payload.len() + 2];
        UnparsedPublicKey::new(&ED25519, key)
            .verify(signed.as_bytes(), &signature)
            .map_err(|_| Refusal::Signature)?;
        let claims: Claims = BASE64_URL_SAFE_NO_PAD
            .decode(payload)
            .ok()
            .and_then(|json| serde_json::from_slice(&json).ok())
            .ok_or(Refusal::Malformed)?;
        let policy = &self.policy;
        if claims.aud != policy.audience {
            return Err(Refusal::Audience);
        }
        if claims.principal.tenant_id() != policy.tenant.as_deref() {
            return Err(Refusal::Tenant);
        }
        if claims.iat > now.saturating_add(policy.skew_ms) {
            return Err(Refusal::NotYetValid);
        }
        if now >= claims.exp.saturating_add(policy.skew_ms) {
            return Err(Refusal::Expired);
        }
        if claims.exp <= claims.iat || claims.exp - claims.iat > policy.max_lifetime_ms {
            return Err(Refusal::Lifetime);
        }
        if now >= claims.principal.expires_unix_ms() {
            return Err(Refusal::CredentialExpired);
        }
        if claims.generation < self.minimum() {
            return Err(Refusal::Stale);
        }
        let grant = claims.principal.grant().ok_or(Refusal::Principal)?;
        self.raise_minimum(claims.generation);
        Ok(Arc::new(grant))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) const TENANT: &str = "0123456789abcdef0123456789abcdef";
    pub(crate) const LINEAGE: u64 = 0x1234;

    pub(crate) fn principal(tenant: Option<&str>, expires: u64) -> credentials::Principal {
        credentials::Principal::of(&credentials::Grant::for_user(
            "abcdefghijkl".into(),
            "gateway test".into(),
            tenant.map(str::to_owned),
            &[credentials::Op::Read, credentials::Op::Collections],
            LINEAGE,
            expires,
        ))
    }

    pub(crate) fn policy(tenant: Option<&str>) -> Policy {
        Policy {
            audience: "backend-a".into(),
            tenant: tenant.map(str::to_owned),
            skew_ms: 1_000,
            max_lifetime_ms: 60_000,
            minimum_generation: 1,
        }
    }

    pub(crate) fn claims(generation: u64, iat: u64, exp: u64) -> Claims {
        Claims {
            aud: "backend-a".into(),
            iat,
            exp,
            generation,
            principal: principal(Some(TENANT), u64::MAX / 2),
        }
    }

    pub(crate) fn signer(kid: &str, byte: u8) -> Signer {
        Signer::from_seed(kid, &[byte; 32]).unwrap()
    }

    fn verifier(keys: &[&Signer]) -> Verifier {
        Verifier::new(
            keys.iter()
                .map(|s| (s.kid.clone(), s.public_key()))
                .collect(),
            policy(Some(TENANT)),
        )
        .unwrap()
    }

    const NOW: u64 = 1_000_000;

    #[test]
    fn valid_envelope_yields_the_scoped_grant() {
        let key = signer("k1", 7);
        let token = key.sign(&claims(3, NOW, NOW + 30_000)).unwrap();
        let grant = verifier(&[&key]).verify(&token, NOW).unwrap();
        assert_eq!(grant.id, "abcdefghijkl");
        assert!(grant.api_credential);
        assert_eq!(grant.tenant_id.as_deref(), Some(TENANT));
        assert_eq!(grant.lineage, LINEAGE);
        assert!(grant.allows(credentials::Op::Read));
        assert!(!grant.allows(credentials::Op::Write));
    }

    #[test]
    fn verifier_refuses_bad_key_sets() {
        let a = signer("k1", 1);
        let b = signer("k2", 2);
        let c = signer("k3", 3);
        let set = |s: &[&Signer]| -> Vec<(String, [u8; 32])> {
            s.iter().map(|s| (s.kid.clone(), s.public_key())).collect()
        };
        assert!(Verifier::new(Vec::new(), policy(None)).is_err());
        assert!(Verifier::new(set(&[&a, &b, &c]), policy(None)).is_err());
        assert!(Verifier::new(set(&[&a, &a]), policy(None)).is_err());
        assert!(Verifier::new(vec![("K!".into(), a.public_key())], policy(None)).is_err());
        assert!(Verifier::new(set(&[&a, &b]), policy(None)).is_ok());
        assert!(Signer::from_seed("Bad.Kid", &[0; 32]).is_err());
    }

    #[test]
    fn rotation_overlap_accepts_old_and_new_and_refuses_unknown() {
        let old = signer("k1", 1);
        let new = signer("k2", 2);
        let other = signer("k3", 3);
        let verifier = verifier(&[&old, &new]);
        for key in [&old, &new] {
            let token = key.sign(&claims(1, NOW, NOW + 1_000)).unwrap();
            assert!(verifier.verify(&token, NOW).is_ok());
        }
        let token = other.sign(&claims(1, NOW, NOW + 1_000)).unwrap();
        assert_eq!(
            verifier.verify(&token, NOW).err(),
            Some(Refusal::UnknownKey)
        );
        // After the overlap ends only the new key remains.
        let after = super::tests::verifier(&[&new]);
        let token = old.sign(&claims(1, NOW, NOW + 1_000)).unwrap();
        assert_eq!(after.verify(&token, NOW).err(), Some(Refusal::UnknownKey));
    }

    #[test]
    fn forged_or_altered_envelopes_are_refused() {
        let key = signer("k1", 1);
        let impostor = signer("k1", 9);
        let verifier = verifier(&[&key]);
        let forged = impostor.sign(&claims(1, NOW, NOW + 1_000)).unwrap();
        assert_eq!(
            verifier.verify(&forged, NOW).err(),
            Some(Refusal::Signature)
        );

        let token = key.sign(&claims(1, NOW, NOW + 1_000)).unwrap();
        let parts: Vec<&str> = token.split('.').collect();
        let mut altered = claims(1, NOW, NOW + 1_000);
        altered.principal = principal(Some(TENANT), u64::MAX / 2);
        altered.generation = 99;
        let payload = BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(&altered).unwrap());
        let spliced = format!("{}.{}.{}.{}", parts[0], parts[1], payload, parts[3]);
        assert_eq!(
            verifier.verify(&spliced, NOW).err(),
            Some(Refusal::Signature)
        );

        for bad in [
            "",
            "vle1",
            "vle2.k1.e30.AA",
            "vle1.k1.!!!.AA",
            "vle1.k1.e30.AA.extra",
            &"x".repeat(MAX_TOKEN_BYTES + 1),
        ] {
            assert!(verifier.verify(bad, NOW).is_err(), "{bad:.20}");
        }
    }

    #[test]
    fn unknown_claims_and_secret_fields_are_refused() {
        let key = signer("k1", 1);
        let verifier = verifier(&[&key]);
        let mut value = serde_json::to_value(claims(1, NOW, NOW + 1_000)).unwrap();
        value["principal"]["secret_sha256"] = json!("00".repeat(32));
        let payload = BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap());
        let signed = format!("{VERSION}.k1.{payload}");
        let signature = BASE64_URL_SAFE_NO_PAD.encode(key.pair.sign(signed.as_bytes()).as_ref());
        let token = format!("{signed}.{signature}");
        assert_eq!(verifier.verify(&token, NOW).err(), Some(Refusal::Malformed));
    }

    #[test]
    fn audience_and_tenant_are_bound() {
        let key = signer("k1", 1);
        let verifier = verifier(&[&key]);
        let mut wrong = claims(1, NOW, NOW + 1_000);
        wrong.aud = "backend-b".into();
        let token = key.sign(&wrong).unwrap();
        assert_eq!(verifier.verify(&token, NOW).err(), Some(Refusal::Audience));

        let mut other = claims(1, NOW, NOW + 1_000);
        other.principal = principal(Some("fedcba9876543210fedcba9876543210"), u64::MAX / 2);
        let token = key.sign(&other).unwrap();
        assert_eq!(verifier.verify(&token, NOW).err(), Some(Refusal::Tenant));

        let mut untenanted = claims(1, NOW, NOW + 1_000);
        untenanted.principal = principal(None, u64::MAX / 2);
        let token = key.sign(&untenanted).unwrap();
        assert_eq!(verifier.verify(&token, NOW).err(), Some(Refusal::Tenant));
    }

    /// TEST-014: skewed clocks around issue and expiry, bounded by `skew_ms`.
    #[test]
    fn expiry_and_issue_time_honour_the_skew_bound_exactly() {
        let key = signer("k1", 1);
        let verifier = verifier(&[&key]);
        let skew = 1_000;
        let token = key.sign(&claims(1, NOW, NOW + 10_000)).unwrap();
        // Backend clock behind the gateway by up to the skew: accepted.
        assert!(verifier.verify(&token, NOW - skew).is_ok());
        assert_eq!(
            verifier.verify(&token, NOW - skew - 1).err(),
            Some(Refusal::NotYetValid)
        );
        // Backend clock ahead: accepted until exp + skew, refused at it.
        assert!(verifier.verify(&token, NOW + 10_000 + skew - 1).is_ok());
        assert_eq!(
            verifier.verify(&token, NOW + 10_000 + skew).err(),
            Some(Refusal::Expired)
        );
        assert_eq!(Refusal::Expired.error().status, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn lifetime_is_bounded_and_positive() {
        let key = signer("k1", 1);
        let verifier = verifier(&[&key]);
        let token = key.sign(&claims(1, NOW, NOW + 60_000)).unwrap();
        assert!(verifier.verify(&token, NOW).is_ok());
        let token = key.sign(&claims(1, NOW, NOW + 60_001)).unwrap();
        assert_eq!(verifier.verify(&token, NOW).err(), Some(Refusal::Lifetime));
        let token = key.sign(&claims(1, NOW, NOW)).unwrap();
        assert_eq!(verifier.verify(&token, NOW).err(), Some(Refusal::Lifetime));
    }

    #[test]
    fn expired_credential_is_refused_even_inside_the_envelope_window() {
        let key = signer("k1", 1);
        let verifier = verifier(&[&key]);
        let mut c = claims(1, NOW, NOW + 10_000);
        c.principal = principal(Some(TENANT), NOW + 5_000);
        let token = key.sign(&c).unwrap();
        assert!(verifier.verify(&token, NOW + 4_999).is_ok());
        assert_eq!(
            verifier.verify(&token, NOW + 5_000).err(),
            Some(Refusal::CredentialExpired)
        );
    }

    /// RD-004: below the minimum `A` seen or told is refused.
    #[test]
    fn generation_floor_rises_and_never_falls() {
        let key = signer("k1", 1);
        let verifier = verifier(&[&key]);
        let at = |a| key.sign(&claims(a, NOW, NOW + 1_000)).unwrap();
        assert_eq!(verifier.verify(&at(0), NOW).err(), Some(Refusal::Stale));
        assert!(verifier.verify(&at(2), NOW).is_ok());
        assert_eq!(verifier.minimum(), 2);
        assert_eq!(verifier.verify(&at(1), NOW).err(), Some(Refusal::Stale));
        verifier.raise_minimum(5);
        verifier.raise_minimum(3);
        assert_eq!(verifier.minimum(), 5);
        assert_eq!(verifier.verify(&at(4), NOW).err(), Some(Refusal::Stale));
        assert!(verifier.verify(&at(5), NOW).is_ok());
        assert_eq!(Refusal::Stale.error().code, "envelope_stale");
    }

    #[test]
    fn envelopes_and_errors_carry_no_key_material() {
        let seed = [0x5au8; 32];
        let key = Signer::from_seed("k1", &seed).unwrap();
        let token = key.sign(&claims(1, NOW, NOW + 1_000)).unwrap();
        let hex_seed = hex(&seed);
        let b64_seed = BASE64_URL_SAFE_NO_PAD.encode(seed);
        assert!(!token.contains(&hex_seed) && !token.contains(&b64_seed));
        let payload = BASE64_URL_SAFE_NO_PAD
            .decode(token.split('.').nth(2).unwrap())
            .unwrap();
        let text = String::from_utf8(payload).unwrap();
        assert!(!text.contains("secret") && !text.contains(&hex_seed));
        for refusal in [Refusal::Signature, Refusal::UnknownKey, Refusal::Malformed] {
            let error = refusal.error();
            assert!(!error.message.contains("k1"));
            assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        }
    }
}
