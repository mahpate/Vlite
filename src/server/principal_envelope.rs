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
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::AtomicU64;

/// The request header a gateway sets and a trusted backend reads.
pub(super) const HEADER: &str = "x-vlite-principal";
const VERSION: &str = "vle1";
const KEY_BUNDLE_SCHEMA_VERSION: u32 = 1;
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
    metadata: SignerMetadata,
}

pub(super) struct GatewayKeyBundle<'a> {
    public_bundle: &'a [u8],
    active_kid: &'a str,
    signing_key: Option<&'a [u8]>,
}

impl Signer {
    /// Builds the gateway signer from a startup-loaded public bundle and
    /// separately delivered signing material. The secret is neither retained
    /// outside `ring` nor exposed by this type.
    pub(super) fn from_gateway_bundle(bundle: GatewayKeyBundle<'_>) -> Result<Self, &'static str> {
        let public_bundle = parse_key_bundle(bundle.public_bundle)?;
        if !valid_kid(bundle.active_kid) {
            return Err("invalid active key identity");
        }
        let active_key = public_bundle
            .keys
            .iter()
            .find(|(kid, _)| kid == bundle.active_kid)
            .map(|(_, key)| key)
            .ok_or("active key is not in the public bundle")?;
        let seed = bundle
            .signing_key
            .ok_or("active signing material is missing")?;
        let seed: &[u8; 32] = seed.try_into().map_err(|_| "invalid signing material")?;
        let pair =
            Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| "invalid signing material")?;
        if pair.public_key().as_ref() != active_key {
            return Err("active signing material does not match public bundle");
        }
        let kids = public_bundle
            .keys
            .iter()
            .map(|(kid, _)| kid.clone())
            .collect();
        Ok(Self {
            kid: bundle.active_kid.to_owned(),
            pair,
            metadata: SignerMetadata {
                generation: public_bundle.generation,
                active_kid: bundle.active_kid.to_owned(),
                kids,
            },
        })
    }

    #[cfg(test)]
    pub(super) fn from_seed(kid: &str, seed: &[u8; 32]) -> Result<Self, &'static str> {
        if !valid_kid(kid) {
            return Err("invalid key identity");
        }
        let pair = Ed25519KeyPair::from_seed_unchecked(seed).map_err(|_| "invalid signing key")?;
        Ok(Self {
            kid: kid.to_owned(),
            pair,
            metadata: SignerMetadata {
                generation: 1,
                active_kid: kid.to_owned(),
                kids: vec![kid.to_owned()],
            },
        })
    }

    pub(super) fn metadata(&self) -> &SignerMetadata {
        &self.metadata
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

/// Safe metadata for a gateway's startup-loaded key bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SignerMetadata {
    pub generation: u64,
    pub active_kid: String,
    pub kids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicKeyBundle {
    schema_version: u32,
    delivery_generation: u64,
    keys: Vec<PublicKeyEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicKeyEntry {
    kid: String,
    public_key: String,
}

struct ParsedKeyBundle {
    generation: u64,
    keys: Vec<(String, [u8; 32])>,
}

fn parse_key_bundle(bundle: &[u8]) -> Result<ParsedKeyBundle, &'static str> {
    let bundle: PublicKeyBundle =
        serde_json::from_slice(bundle).map_err(|_| "invalid public key bundle")?;
    if bundle.schema_version != KEY_BUNDLE_SCHEMA_VERSION || bundle.delivery_generation == 0 {
        return Err("unsupported public key bundle");
    }
    if !(1..=2).contains(&bundle.keys.len()) {
        return Err("public key bundle must contain one or two keys");
    }
    let mut seen = HashSet::with_capacity(bundle.keys.len());
    let mut keys = Vec::with_capacity(bundle.keys.len());
    for entry in bundle.keys {
        if !valid_kid(&entry.kid) || !seen.insert(entry.kid.clone()) {
            return Err("invalid or duplicate key identity");
        }
        let decoded = BASE64_URL_SAFE_NO_PAD
            .decode(entry.public_key)
            .map_err(|_| "invalid public key bundle")?;
        let key: [u8; 32] = decoded
            .try_into()
            .map_err(|_| "invalid public key bundle")?;
        keys.push((entry.kid, key));
    }
    Ok(ParsedKeyBundle {
        generation: bundle.delivery_generation,
        keys,
    })
}

/// Safe metadata for a backend's startup-loaded public key bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct VerifierMetadata {
    pub generation: u64,
    pub kids: Vec<String>,
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
    key_generation: u64,
    policy: Policy,
    /// Highest `A` accepted or announced; never decreases.
    floor: AtomicU64,
}

impl Verifier {
    /// Loads a validated, immutable public-key bundle at backend startup.
    /// Retired identities must be absent; no other key is tried on a miss.
    pub(super) fn from_bundle(bundle: &[u8], policy: Policy) -> Result<Self, &'static str> {
        let bundle = parse_key_bundle(bundle)?;
        Self::from_keys(bundle.keys, bundle.generation, policy)
    }

    #[cfg(test)]
    pub(super) fn new(keys: Vec<(String, [u8; 32])>, policy: Policy) -> Result<Self, &'static str> {
        Self::from_keys(keys, 1, policy)
    }

    fn from_keys(
        keys: Vec<(String, [u8; 32])>,
        key_generation: u64,
        policy: Policy,
    ) -> Result<Self, &'static str> {
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
            key_generation,
            policy,
            floor: AtomicU64::new(0),
        })
    }

    pub(super) fn metadata(&self) -> VerifierMetadata {
        VerifierMetadata {
            generation: self.key_generation,
            kids: self.keys.iter().map(|(kid, _)| kid.clone()).collect(),
        }
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

    fn public_bundle(keys: &[&Signer], generation: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "delivery_generation": generation,
            "keys": keys.iter().map(|key| json!({
                "kid": key.kid,
                "public_key": BASE64_URL_SAFE_NO_PAD.encode(key.public_key()),
            })).collect::<Vec<_>>(),
        }))
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
        let verifier =
            Verifier::from_bundle(&public_bundle(&[&old, &new], 2), policy(Some(TENANT))).unwrap();
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
        let after =
            Verifier::from_bundle(&public_bundle(&[&new], 3), policy(Some(TENANT))).unwrap();
        let token = old.sign(&claims(1, NOW, NOW + 1_000)).unwrap();
        assert_eq!(after.verify(&token, NOW).err(), Some(Refusal::UnknownKey));
    }

    #[test]
    fn startup_bundle_is_validated_and_exposes_only_safe_metadata() {
        let old = signer("k1", 1);
        let new = signer("k2", 2);
        let bundle = public_bundle(&[&old, &new], 17);
        let verifier = Verifier::from_bundle(&bundle, policy(Some(TENANT))).unwrap();
        let metadata = verifier.metadata();
        assert_eq!(metadata.generation, 17);
        assert_eq!(metadata.kids, ["k1", "k2"]);
        let metadata_debug = format!("{metadata:?}");
        assert!(!metadata_debug.contains(&BASE64_URL_SAFE_NO_PAD.encode(old.public_key())));

        let claims = claims(1, NOW, NOW + 1_000);
        let token = old.sign(&claims).unwrap();
        assert!(verifier.verify(&token, NOW).is_ok());
        assert_eq!(verifier.minimum(), 1);

        let duplicate = serde_json::to_vec(&json!({
            "schema_version": 1,
            "delivery_generation": 18,
            "keys": [
                {"kid": "k1", "public_key": BASE64_URL_SAFE_NO_PAD.encode(old.public_key())},
                {"kid": "k1", "public_key": BASE64_URL_SAFE_NO_PAD.encode(new.public_key())},
            ],
        }))
        .unwrap();
        assert!(Verifier::from_bundle(&duplicate, policy(None)).is_err());
        assert!(Verifier::from_bundle(b"{", policy(None)).is_err());
        assert!(
            Verifier::from_bundle(
                br#"{"schema_version":2,"delivery_generation":1,"keys":[]}"#,
                policy(None)
            )
            .is_err()
        );
        assert!(
            Verifier::from_bundle(
                br#"{"schema_version":1,"delivery_generation":0,"keys":[]}"#,
                policy(None)
            )
            .is_err()
        );
    }

    #[test]
    fn gateway_startup_requires_a_matching_active_signing_key() {
        let active = signer("k1", 9);
        let other = signer("k2", 7);
        let bundle = public_bundle(&[&active, &other], 4);
        let seed = [9u8; 32];
        let gateway = Signer::from_gateway_bundle(GatewayKeyBundle {
            public_bundle: &bundle,
            active_kid: "k1",
            signing_key: Some(&seed),
        })
        .unwrap();
        assert_eq!(gateway.metadata().generation, 4);
        assert_eq!(gateway.metadata().active_kid, "k1");
        assert_eq!(gateway.metadata().kids, ["k1", "k2"]);
        let metadata_debug = format!("{:?}", gateway.metadata());
        assert!(!metadata_debug.contains(&BASE64_URL_SAFE_NO_PAD.encode(seed)));
        let token = gateway.sign(&claims(1, NOW, NOW + 1_000)).unwrap();
        assert!(verifier(&[&active, &other]).verify(&token, NOW).is_ok());

        assert!(
            Signer::from_gateway_bundle(GatewayKeyBundle {
                public_bundle: &bundle,
                active_kid: "k1",
                signing_key: None,
            })
            .is_err()
        );
        assert!(
            Signer::from_gateway_bundle(GatewayKeyBundle {
                public_bundle: &bundle,
                active_kid: "unknown",
                signing_key: Some(&seed),
            })
            .is_err()
        );
        let wrong_seed = [3u8; 32];
        assert!(
            Signer::from_gateway_bundle(GatewayKeyBundle {
                public_bundle: &bundle,
                active_kid: "k1",
                signing_key: Some(&wrong_seed),
            })
            .is_err()
        );
        assert!(
            Signer::from_gateway_bundle(GatewayKeyBundle {
                public_bundle: &bundle,
                active_kid: "k1",
                signing_key: Some(b"short"),
            })
            .is_err()
        );
    }

    #[test]
    fn key_identity_is_signed_and_retired_keys_are_not_fallback_candidates() {
        let old = signer("k1", 1);
        let new = signer("k2", 2);
        let token = old.sign(&claims(1, NOW, NOW + 1_000)).unwrap();
        let mut parts: Vec<_> = token.split('.').map(str::to_owned).collect();
        parts[1] = "k2".into();
        let changed_kid = parts.join(".");
        assert_eq!(
            verifier(&[&old, &new]).verify(&changed_kid, NOW).err(),
            Some(Refusal::Signature)
        );

        let after_retirement = verifier(&[&new]);
        assert_eq!(
            after_retirement.verify(&token, NOW).err(),
            Some(Refusal::UnknownKey)
        );
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
        for error in [
            Verifier::from_bundle(b"{", policy(None)).err().unwrap(),
            Signer::from_gateway_bundle(GatewayKeyBundle {
                public_bundle: b"{}",
                active_kid: "k1",
                signing_key: Some(&seed),
            })
            .err()
            .unwrap(),
            Signer::from_gateway_bundle(GatewayKeyBundle {
                public_bundle: b"{}",
                active_kid: "k1",
                signing_key: None,
            })
            .err()
            .unwrap(),
            Signer::from_gateway_bundle(GatewayKeyBundle {
                public_bundle: &public_bundle(&[&key], 2),
                active_kid: "k1",
                signing_key: Some(&[1u8; 32]),
            })
            .err()
            .unwrap(),
        ] {
            assert!(!error.contains(&hex_seed));
            assert!(!error.contains(&BASE64_URL_SAFE_NO_PAD.encode(seed)));
            assert!(!error.contains(&BASE64_URL_SAFE_NO_PAD.encode([1u8; 32])));
        }
    }
}
