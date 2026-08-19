//! Verifying a JWT somebody else issued.
//!
//! The other half of [`crate::auth`]'s bearer story. An app-token is a
//! credential *this* LB minted and can look up; a JWT is a credential another
//! service signed, which app-lb has never seen before and cannot ask anyone
//! about. Everything here follows from that: the token has to carry its own
//! proof, and the only thing app-lb brings to the check is a key and a policy.
//!
//! The immediate reason it exists is the Heyo auth API, which hands its users an
//! HS256 token with `userId`/`email`/`role` claims, an `auth-service` issuer and
//! a `heyo-app` audience. But nothing here is shaped around that: the algorithm,
//! the key, the issuer, the audience, which claim is the subject and which
//! claims must hold are all configuration, so the same gate fronts an Auth0,
//! Okta, Cognito or Keycloak deployment without a line of code.
//!
//! ## The algorithm allow-list is the whole security story
//!
//! A JWT header names the algorithm it was signed with, and the header is part
//! of the *token* — which is to say, part of the attacker's input. Two attacks
//! follow, and both are defended here by never letting the token choose:
//!
//! * **`alg: none`.** The original JWT specification made the signature
//!   optional. A verifier that dispatches on the header will happily conclude
//!   that an unsigned token verified. [`Algorithm::parse`] has no `none` arm, so
//!   such a token does not get as far as a signature check.
//! * **Algorithm confusion.** A gate holding an RSA *public* key can be handed a
//!   token whose header says `HS256`, signed with that public key as the HMAC
//!   secret — the key is public, so anyone can do it. A verifier that reads the
//!   header and picks HMAC accepts it. Here the configured `algorithms` decide
//!   what is acceptable *and* [`Key`] carries its own family, so an HMAC
//!   algorithm cannot be verified against an asymmetric key or the reverse. Both
//!   checks are redundant with each other on purpose.
//!
//! Which is why `algorithms` is required in the spec rather than defaulted:
//! "whatever the token says" is not a policy, and "whatever the key looks like"
//! is a guess.
//!
//! ## What is checked, in order
//!
//! 1. The header's `alg` is one the gate configured.
//! 2. The signature, against the configured key — or, for JWKS, the key whose
//!    `kid` the header names.
//! 3. `exp`, and `nbf` when present, with the configured leeway.
//! 4. `iss` — exactly, and always, because a signature only proves *a* holder of
//!    the key signed this, and with a shared secret that includes every other
//!    service the secret was issued to.
//! 5. `aud`, when the gate names one.
//! 6. The gate's `require` claims.
//!
//! An expired token and a forged one produce the same refusal to the caller: the
//! detail goes to the log and the SIEM, never to whoever presented it.

use crate::config::JwtSpec;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Public};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a fetched key set is used before it is refreshed.
///
/// Ten minutes, which is what the major issuers' own `Cache-Control` suggests
/// and short enough that a key rotated on a schedule is picked up without
/// anybody doing anything.
const JWKS_TTL: Duration = Duration::from_secs(600);

/// Floor on how often one URL may be fetched, however many tokens arrive
/// carrying an unknown `kid`.
///
/// Without it a single forged token naming a `kid` that does not exist turns
/// every request into an outbound HTTP request to the issuer — a request
/// amplifier pointed at somebody else's infrastructure, triggerable by anyone
/// who can reach a gated deployment.
const JWKS_MIN_REFETCH: Duration = Duration::from_secs(60);

/// Ceiling on one key-set fetch. A verification blocks a request, so this is a
/// latency budget: a slow issuer must fail the request, not hold the connection.
const JWKS_TIMEOUT: Duration = Duration::from_secs(5);

/// Cap on a key-set document. A JWKS is a handful of keys; anything at this size
/// is a misconfigured URL, and parsing it would be the point of the exercise for
/// whoever pointed the gate at it.
const JWKS_MAX_BYTES: usize = 256 * 1024;

/// The signature algorithms a gate may name.
///
/// Deliberately not exhaustive of the JWA registry. Everything here is
/// verifiable with the OpenSSL already in the tree, and everything left out is
/// left out because nothing app-lb is likely to front issues it — adding one is
/// a match arm and a test, not a design change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Algorithm {
    Hs256,
    Hs384,
    Hs512,
    Rs256,
    Rs384,
    Rs512,
    Ps256,
    Ps384,
    Ps512,
    Es256,
    Es384,
}

impl Algorithm {
    /// The JWA name, or `None` for anything unrecognised — `none` included,
    /// which is the point: there is no arm for it, so an unsigned token cannot
    /// be described by this type at all.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "HS256" => Self::Hs256,
            "HS384" => Self::Hs384,
            "HS512" => Self::Hs512,
            "RS256" => Self::Rs256,
            "RS384" => Self::Rs384,
            "RS512" => Self::Rs512,
            "PS256" => Self::Ps256,
            "PS384" => Self::Ps384,
            "PS512" => Self::Ps512,
            "ES256" => Self::Es256,
            "ES384" => Self::Es384,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hs256 => "HS256",
            Self::Hs384 => "HS384",
            Self::Hs512 => "HS512",
            Self::Rs256 => "RS256",
            Self::Rs384 => "RS384",
            Self::Rs512 => "RS512",
            Self::Ps256 => "PS256",
            Self::Ps384 => "PS384",
            Self::Ps512 => "PS512",
            Self::Es256 => "ES256",
            Self::Es384 => "ES384",
        }
    }

    /// Whether this algorithm is keyed by a shared secret rather than a key
    /// pair. The whole of the algorithm-confusion defence rests on this
    /// question being asked of the *configured* algorithm and the *configured*
    /// key, never of the token.
    pub fn is_symmetric(self) -> bool {
        matches!(self, Self::Hs256 | Self::Hs384 | Self::Hs512)
    }

    fn digest(self) -> MessageDigest {
        match self {
            Self::Hs256 | Self::Rs256 | Self::Ps256 | Self::Es256 => MessageDigest::sha256(),
            Self::Hs384 | Self::Rs384 | Self::Ps384 | Self::Es384 => MessageDigest::sha384(),
            Self::Hs512 | Self::Rs512 | Self::Ps512 => MessageDigest::sha512(),
        }
    }

    /// Width of each half of a raw ECDSA signature, which is the coordinate size
    /// of the curve — 32 bytes for P-256, 48 for P-384.
    fn ecdsa_half(self) -> Option<usize> {
        match self {
            Self::Es256 => Some(32),
            Self::Es384 => Some(48),
            _ => None,
        }
    }
}

/// Key material a gate verifies with.
///
/// Carries its own family so [`verify_signature`] can refuse a symmetric
/// algorithm against an asymmetric key without consulting anything the token
/// said. See the module note on algorithm confusion.
pub enum Key {
    /// An HMAC shared secret, as it came out of the secret store.
    Secret(Vec<u8>),
    /// A public key, parsed from the spec's PEM or built from a JWK.
    Public(PKey<Public>),
}

/// Why a token did not admit its bearer.
///
/// Every variant is a *log* message. What the caller is told is one 401 with no
/// detail at all, because the difference between "expired" and "signed with the
/// wrong key" is precisely the feedback an attacker is looking for.
#[derive(Debug, PartialEq, Eq)]
pub enum JwtError {
    /// Not three base64url segments, or a header/payload that is not JSON.
    Malformed(&'static str),
    /// The header named an algorithm the gate does not accept — or one that does
    /// not exist, `none` included.
    Algorithm(String),
    /// The gate is configured with a key that cannot verify the algorithm the
    /// token was signed with. A misconfiguration, or an attempt at confusion.
    KeyMismatch,
    /// The header's `kid` is in no key set this gate can reach.
    UnknownKey(String),
    /// The key set could not be fetched.
    Jwks(String),
    /// The signature did not verify.
    Signature,
    /// `exp` is in the past, `nbf` in the future, or `exp` is missing entirely.
    Expiry(&'static str),
    /// `iss` or `aud` did not match.
    Claim(&'static str),
    /// A `require` claim was absent or held something else.
    Require(String),
    /// The key material itself is unusable — a PEM that will not parse, a JWK
    /// with a bad modulus.
    Key(String),
}

impl std::fmt::Display for JwtError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(what) => write!(f, "the token is malformed: {what}"),
            Self::Algorithm(alg) => write!(
                f,
                "the token is signed with {alg:?}, which this gate does not accept"
            ),
            Self::KeyMismatch => write!(
                f,
                "the token's algorithm does not match the kind of key this gate holds — a \
                 shared secret cannot verify an asymmetric signature, and a public key must \
                 not be used as an HMAC secret"
            ),
            Self::UnknownKey(kid) => write!(f, "no key with kid {kid:?} in the issuer's key set"),
            Self::Jwks(e) => write!(f, "could not read the issuer's key set: {e}"),
            Self::Signature => write!(f, "the signature did not verify"),
            Self::Expiry(what) => write!(f, "{what}"),
            Self::Claim(what) => write!(f, "{what}"),
            Self::Require(claim) => write!(f, "the {claim} claim does not satisfy this gate"),
            Self::Key(e) => write!(f, "the gate's key is unusable: {e}"),
        }
    }
}

impl std::error::Error for JwtError {}

/// A token's header, as much of it as this needs.
#[derive(Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: Option<String>,
}

/// A verified token's payload.
///
/// Kept as the raw claim map rather than a struct, because which claims matter
/// is the gate's business: one issuer's subject is `sub`, another's is `userId`,
/// and a `require` can name anything at all.
pub struct Claims(Map<String, Value>);

impl std::fmt::Debug for Claims {
    /// Names the claims, never their values. A token's payload belongs to
    /// whoever presented it, but it is somebody's identity — an address, a user
    /// id, an account — and a `{:?}` in a log line is not where that should turn
    /// up.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Claims")
            .field("claims", &self.0.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl Claims {
    /// A claim as a string. Numbers and booleans render as themselves, so a
    /// numeric user id can still be a subject; anything structural is `None`.
    pub fn string(&self, name: &str) -> Option<String> {
        match self.0.get(name)? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name)
    }
}

/// Verify a token against a gate's policy, and return its claims.
///
/// `now` is passed rather than read so the expiry rules are testable without
/// waiting; every caller in the data plane passes [`crate::deployment::now_secs`].
pub async fn verify(
    token: &str,
    spec: &JwtSpec,
    key: Option<&Key>,
    jwks: &JwksCache,
    now: u64,
) -> Result<Claims, JwtError> {
    let (signed, signature_b64) = token
        .rsplit_once('.')
        .ok_or(JwtError::Malformed("expected three dot-separated segments"))?;
    let (header_b64, payload_b64) = signed
        .split_once('.')
        .ok_or(JwtError::Malformed("expected three dot-separated segments"))?;
    if payload_b64.contains('.') {
        return Err(JwtError::Malformed("expected three dot-separated segments"));
    }

    let header: Header = serde_json::from_slice(&decode(header_b64, "header")?)
        .map_err(|_| JwtError::Malformed("the header is not JSON"))?;

    // The gate's list decides, not the token's claim about itself.
    let alg = Algorithm::parse(&header.alg).filter(|a| spec.accepts(*a));
    let alg = alg.ok_or_else(|| JwtError::Algorithm(header.alg.clone()))?;

    // Resolved before the signature check so a JWKS failure is reported as
    // itself rather than as an unverifiable signature.
    let key = match key {
        Some(k) => KeyRef::Borrowed(k),
        None => KeyRef::Owned(Key::Public(
            jwks.key_for(spec.jwks_url(), header.kid.as_deref(), now).await?,
        )),
    };

    let signature = decode(signature_b64, "signature")?;
    verify_signature(alg, key.get(), signed.as_bytes(), &signature)?;

    let claims: Map<String, Value> = serde_json::from_slice(&decode(payload_b64, "payload")?)
        .map_err(|_| JwtError::Malformed("the payload is not a JSON object"))?;
    let claims = Claims(claims);
    check_claims(&claims, spec, now)?;
    Ok(claims)
}

/// A key that is either the gate's own or one this verification just fetched.
/// Exists so the JWKS path does not have to clone a `PKey` into the caller's
/// lifetime, and the static path does not have to clone at all.
enum KeyRef<'a> {
    Borrowed(&'a Key),
    Owned(Key),
}

impl KeyRef<'_> {
    fn get(&self) -> &Key {
        match self {
            Self::Borrowed(k) => k,
            Self::Owned(k) => k,
        }
    }
}

fn decode(segment: &str, what: &'static str) -> Result<Vec<u8>, JwtError> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(segment)
        .map_err(|_| match what {
            "header" => JwtError::Malformed("the header is not base64url"),
            "payload" => JwtError::Malformed("the payload is not base64url"),
            _ => JwtError::Malformed("the signature is not base64url"),
        })
}

/// Check the signature over `signed` — the header and payload segments with the
/// dot between them, exactly as they arrived. Re-encoding either would change
/// the bytes that were signed.
fn verify_signature(
    alg: Algorithm,
    key: &Key,
    signed: &[u8],
    signature: &[u8],
) -> Result<(), JwtError> {
    match (alg.is_symmetric(), key) {
        (true, Key::Secret(secret)) => {
            let pkey = PKey::hmac(secret).map_err(|e| JwtError::Key(e.to_string()))?;
            let mut signer = openssl::sign::Signer::new(alg.digest(), &pkey)
                .map_err(|e| JwtError::Key(e.to_string()))?;
            signer.update(signed).map_err(|_| JwtError::Signature)?;
            let expected = signer.sign_to_vec().map_err(|_| JwtError::Signature)?;
            // Constant-time: a byte-at-a-time comparison of a MAC is the classic
            // way to have the signature handed to you a byte at a time.
            crate::auth::ct_eq(signature, &expected)
                .then_some(())
                .ok_or(JwtError::Signature)
        }
        (false, Key::Public(public)) => {
            // A raw ECDSA signature is r‖s at fixed width; OpenSSL verifies the
            // DER encoding. The conversion is not cosmetic — handing the raw
            // form to `Verifier` fails every time.
            let der;
            let signature = match alg.ecdsa_half() {
                Some(half) => {
                    der = ecdsa_der(signature, half)?;
                    der.as_slice()
                }
                None => signature,
            };

            let mut verifier = openssl::sign::Verifier::new(alg.digest(), public)
                .map_err(|e| JwtError::Key(e.to_string()))?;
            if matches!(alg, Algorithm::Ps256 | Algorithm::Ps384 | Algorithm::Ps512) {
                verifier
                    .set_rsa_padding(openssl::rsa::Padding::PKCS1_PSS)
                    .and_then(|()| verifier.set_rsa_mgf1_md(alg.digest()))
                    // RFC 7518 §3.5: the salt is the length of the hash.
                    .and_then(|()| {
                        verifier.set_rsa_pss_saltlen(openssl::sign::RsaPssSaltlen::DIGEST_LENGTH)
                    })
                    .map_err(|e| JwtError::Key(e.to_string()))?;
            }
            verifier.update(signed).map_err(|_| JwtError::Signature)?;
            match verifier.verify(signature) {
                Ok(true) => Ok(()),
                _ => Err(JwtError::Signature),
            }
        }
        // Either an asymmetric algorithm with a shared secret, or — the one that
        // matters — a symmetric algorithm with a public key.
        _ => Err(JwtError::KeyMismatch),
    }
}

/// `r‖s` → the DER `SEQUENCE { INTEGER r, INTEGER s }` OpenSSL expects.
fn ecdsa_der(signature: &[u8], half: usize) -> Result<Vec<u8>, JwtError> {
    if signature.len() != half * 2 {
        return Err(JwtError::Signature);
    }
    let (r, s) = signature.split_at(half);
    let r = openssl::bn::BigNum::from_slice(r).map_err(|_| JwtError::Signature)?;
    let s = openssl::bn::BigNum::from_slice(s).map_err(|_| JwtError::Signature)?;
    openssl::ecdsa::EcdsaSig::from_private_components(r, s)
        .and_then(|sig| sig.to_der())
        .map_err(|_| JwtError::Signature)
}

/// Expiry, issuer, audience and the gate's `require` map.
fn check_claims(claims: &Claims, spec: &JwtSpec, now: u64) -> Result<(), JwtError> {
    let leeway = spec.leeway_secs();

    // Required, not optional. A token with no expiry is a credential that never
    // stops working, and a gate cannot revoke one it did not issue: the only
    // thing standing between a leaked token and permanent access is `exp`.
    let exp = claims
        .get("exp")
        .and_then(Value::as_u64)
        .ok_or(JwtError::Expiry(
            "the token carries no exp claim, so it would never expire and this gate cannot \
             revoke it",
        ))?;
    if now.saturating_sub(leeway) >= exp {
        return Err(JwtError::Expiry("the token has expired"));
    }
    if let Some(nbf) = claims.get("nbf").and_then(Value::as_u64)
        && now + leeway < nbf
    {
        return Err(JwtError::Expiry("the token is not valid yet"));
    }

    if claims.string("iss").as_deref() != Some(spec.issuer.as_str()) {
        return Err(JwtError::Claim(
            "the token was issued by somebody other than this gate's issuer",
        ));
    }

    // `aud` is a string or an array of them (RFC 7519 §4.1.3), and both spellings
    // are in the wild for the same issuer.
    if let Some(expected) = &spec.audience {
        let ok = match claims.get("aud") {
            Some(Value::String(s)) => s == expected,
            Some(Value::Array(vs)) => vs.iter().any(|v| v.as_str() == Some(expected.as_str())),
            _ => false,
        };
        if !ok {
            return Err(JwtError::Claim("the token is for a different audience"));
        }
    }

    for (claim, wanted) in &spec.require {
        let Some(actual) = claims.get(claim) else {
            return Err(JwtError::Require(claim.clone()));
        };
        if !satisfies(actual, wanted) {
            return Err(JwtError::Require(claim.clone()));
        }
    }
    Ok(())
}

/// Whether a claim's value satisfies a `require` entry.
///
/// Four shapes, and they are the four that come up:
///
/// * scalar vs scalar — equality (`"role": "admin"`).
/// * scalar claim vs list — membership (`"role": ["admin", "owner"]`), so a
///   requirement is an OR over its own values.
/// * list claim vs scalar — the claim contains it (`"scopes": "deploy"` against
///   `"scopes": ["read", "deploy"]`), which is how every scope claim works.
/// * list vs list — they intersect.
///
/// Across claims the map is an AND: every entry must hold. That split — OR
/// inside, AND across — is the one that lets a policy be written without
/// nesting, and it is the same shape an allow-list has.
fn satisfies(actual: &Value, wanted: &Value) -> bool {
    let wanted: Vec<&Value> = match wanted {
        Value::Array(vs) => vs.iter().collect(),
        single => vec![single],
    };
    match actual {
        Value::Array(held) => wanted.iter().any(|w| held.contains(w)),
        single => wanted.contains(&single),
    }
}

// ---------------------------------------------------------------------------
// JWKS
// ---------------------------------------------------------------------------

/// One issuer's published key set, cached.
///
/// Held by the [`crate::auth::Authenticator`] and shared by every gate, so two
/// deployments fronted by the same issuer fetch its keys once.
pub struct JwksCache {
    /// Per-URL, so a fetch for one issuer does not block verification against
    /// another. The outer lock is only ever held long enough to clone an `Arc`
    /// — never across the fetch.
    sets: Mutex<HashMap<String, Arc<tokio::sync::Mutex<KeySet>>>>,
    http: reqwest::Client,
}

#[derive(Default)]
struct KeySet {
    keys: Vec<Jwk>,
    /// When the current contents were fetched. Zero means never.
    fetched_at: u64,
    /// When a fetch was last *attempted*, successful or not. What
    /// [`JWKS_MIN_REFETCH`] is measured against, so a failing issuer is not
    /// retried per request.
    attempted_at: u64,
}

/// One key from a JWKS document, in the two families worth supporting.
#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    kty: String,
    /// RSA modulus, base64url.
    #[serde(default)]
    n: Option<String>,
    /// RSA exponent, base64url.
    #[serde(default)]
    e: Option<String>,
    /// EC curve name.
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

#[derive(Deserialize)]
struct JwksDocument {
    #[serde(default)]
    keys: Vec<Jwk>,
}

impl Default for JwksCache {
    fn default() -> Self {
        Self::new()
    }
}

impl JwksCache {
    pub fn new() -> Self {
        Self {
            sets: Mutex::new(HashMap::new()),
            http: reqwest::Client::builder()
                .timeout(JWKS_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }

    /// The key a token's `kid` names, fetching or refreshing the set if needed.
    ///
    /// A token with no `kid` is served the set's only key, and refused when
    /// there is more than one — guessing which of several keys an unlabelled
    /// token meant would mean trying each of them, and "the signature verified
    /// under *some* key" is a weaker statement than it looks.
    pub async fn key_for(
        &self,
        url: &str,
        kid: Option<&str>,
        now: u64,
    ) -> Result<PKey<Public>, JwtError> {
        let entry = {
            let mut sets = self.sets.lock().expect("jwks cache mutex poisoned");
            sets.entry(url.to_string()).or_default().clone()
        };
        let mut set = entry.lock().await;

        let stale = now.saturating_sub(set.fetched_at) >= JWKS_TTL.as_secs();
        let missing = pick(&set.keys, kid).is_none();
        // A `kid` nobody has seen before is the signal that the issuer rotated,
        // and the only one there is — but it is also attacker-controlled, so the
        // refetch it triggers is rate-limited like any other.
        if (stale || missing) && now.saturating_sub(set.attempted_at) >= JWKS_MIN_REFETCH.as_secs()
        {
            set.attempted_at = now;
            match self.fetch(url).await {
                Ok(keys) => {
                    set.keys = keys;
                    set.fetched_at = now;
                }
                // A fetch that fails over a set already in hand is not fatal:
                // the cached keys are still the issuer's, and refusing every
                // request because the key endpoint blipped would take the
                // deployment down with it.
                Err(e) if !set.keys.is_empty() => {
                    tracing::warn!(url, error = %e, "could not refresh the issuer's key set; using the cached one");
                }
                Err(e) => return Err(JwtError::Jwks(e)),
            }
        }

        let jwk = pick(&set.keys, kid).ok_or_else(|| {
            JwtError::UnknownKey(kid.unwrap_or("(the token names no kid)").to_string())
        })?;
        jwk.to_public_key()
    }

    async fn fetch(&self, url: &str) -> Result<Vec<Jwk>, String> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("{url}: HTTP {status}"));
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| format!("{url}: {e}"))?;
        if body.len() > JWKS_MAX_BYTES {
            return Err(format!(
                "{url}: {} bytes is not a key set",
                body.len()
            ));
        }
        let doc: JwksDocument =
            serde_json::from_slice(&body).map_err(|e| format!("{url}: not a JWKS document: {e}"))?;
        if doc.keys.is_empty() {
            return Err(format!("{url}: the key set is empty"));
        }
        Ok(doc.keys)
    }

    /// Seed a URL's keys without fetching. For tests, and for nothing else — the
    /// data plane learns keys by fetching them.
    #[cfg(test)]
    pub async fn seed(&self, url: &str, document: &str, now: u64) {
        let doc: JwksDocument = serde_json::from_str(document).expect("test JWKS must parse");
        let entry = {
            let mut sets = self.sets.lock().expect("jwks cache mutex poisoned");
            sets.entry(url.to_string()).or_default().clone()
        };
        let mut set = entry.lock().await;
        set.keys = doc.keys;
        set.fetched_at = now;
        set.attempted_at = now;
    }
}

/// The key a `kid` names, or the only key when the token names none.
fn pick<'a>(keys: &'a [Jwk], kid: Option<&str>) -> Option<&'a Jwk> {
    match kid {
        Some(kid) => keys.iter().find(|k| k.kid.as_deref() == Some(kid)),
        None => match keys {
            [only] => Some(only),
            _ => None,
        },
    }
}

impl Jwk {
    /// Build a verifying key from the JWK's components.
    fn to_public_key(&self) -> Result<PKey<Public>, JwtError> {
        let bn = |field: &str, value: &Option<String>| -> Result<openssl::bn::BigNum, JwtError> {
            use base64::Engine;
            let raw = value
                .as_deref()
                .ok_or_else(|| JwtError::Key(format!("the JWK has no {field}")))?;
            let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(raw)
                .map_err(|_| JwtError::Key(format!("the JWK's {field} is not base64url")))?;
            openssl::bn::BigNum::from_slice(&bytes)
                .map_err(|e| JwtError::Key(format!("the JWK's {field} is unusable: {e}")))
        };

        match self.kty.as_str() {
            "RSA" => {
                let rsa = openssl::rsa::Rsa::from_public_components(bn("n", &self.n)?, bn("e", &self.e)?)
                    .map_err(|e| JwtError::Key(e.to_string()))?;
                PKey::from_rsa(rsa).map_err(|e| JwtError::Key(e.to_string()))
            }
            "EC" => {
                let curve = match self.crv.as_deref() {
                    Some("P-256") => openssl::nid::Nid::X9_62_PRIME256V1,
                    Some("P-384") => openssl::nid::Nid::SECP384R1,
                    other => {
                        return Err(JwtError::Key(format!(
                            "unsupported EC curve {:?}",
                            other.unwrap_or("(none)")
                        )));
                    }
                };
                let group = openssl::ec::EcGroup::from_curve_name(curve)
                    .map_err(|e| JwtError::Key(e.to_string()))?;
                let (x, y) = (bn("x", &self.x)?, bn("y", &self.y)?);
                let ec = openssl::ec::EcKey::from_public_key_affine_coordinates(&group, &x, &y)
                    .map_err(|e| JwtError::Key(e.to_string()))?;
                PKey::from_ec_key(ec).map_err(|e| JwtError::Key(e.to_string()))
            }
            other => Err(JwtError::Key(format!("unsupported JWK key type {other:?}"))),
        }
    }
}

/// Parse a PEM public key from a spec, so a bad one is a registration error
/// rather than a 401 on every request.
///
/// Accepts a bare public key (`BEGIN PUBLIC KEY`) and a certificate, because
/// half the issuers that publish a key publish it wrapped in one.
pub fn public_key_from_pem(pem: &str) -> Result<PKey<Public>, String> {
    let pem = pem.trim();
    if pem.contains("BEGIN CERTIFICATE") {
        return openssl::x509::X509::from_pem(pem.as_bytes())
            .and_then(|cert| cert.public_key())
            .map_err(|e| format!("not a usable certificate: {e}"));
    }
    PKey::public_key_from_pem(pem.as_bytes())
        .map_err(|e| format!("not a usable PEM public key: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    /// Assemble a token from a header and payload, signed by `sign`.
    ///
    /// Hand-rolled rather than reached for from a crate on purpose: half these
    /// tests are about tokens a well-behaved library would refuse to *produce*,
    /// which is exactly the input a verifier has to survive.
    fn token(header: serde_json::Value, payload: serde_json::Value, sign: impl Fn(&[u8]) -> Vec<u8>) -> String {
        let signed = format!(
            "{}.{}",
            b64(header.to_string().as_bytes()),
            b64(payload.to_string().as_bytes())
        );
        format!("{signed}.{}", b64(&sign(signed.as_bytes())))
    }

    fn hmac_with(secret: &[u8], digest: MessageDigest) -> impl Fn(&[u8]) -> Vec<u8> + '_ {
        move |data: &[u8]| {
            let key = PKey::hmac(secret).unwrap();
            let mut signer = openssl::sign::Signer::new(digest, &key).unwrap();
            signer.update(data).unwrap();
            signer.sign_to_vec().unwrap()
        }
    }

    const SECRET: &[u8] = b"a-shared-secret-of-some-length";
    const NOW: u64 = 1_800_000_000;

    /// The Heyo auth API's own shape: HS256, `auth-service`/`heyo-app`, and a
    /// `userId` where most issuers put `sub`.
    fn heyo_spec() -> JwtSpec {
        serde_json::from_value(json!({
            "secret": {"secret": "heyo-auth", "key": "jwt_secret"},
            "algorithms": ["HS256"],
            "issuer": "auth-service",
            "audience": "heyo-app",
            "subject_claim": "userId",
        }))
        .unwrap()
    }

    fn heyo_claims() -> serde_json::Value {
        json!({
            "userId": "u_1f2e",
            "email": "someone@example.com",
            "role": "admin",
            "accountId": "acct_7f3c",
            "iss": "auth-service",
            "aud": "heyo-app",
            "iat": NOW - 60,
            "exp": NOW + 3600,
        })
    }

    fn hs256(payload: serde_json::Value) -> String {
        token(
            json!({"alg": "HS256", "typ": "JWT"}),
            payload,
            hmac_with(SECRET, MessageDigest::sha256()),
        )
    }

    async fn check(token: &str, spec: &JwtSpec) -> Result<Claims, JwtError> {
        verify(token, spec, Some(&Key::Secret(SECRET.to_vec())), &JwksCache::new(), NOW).await
    }

    #[tokio::test]
    async fn a_token_from_the_heyo_auth_api_verifies() {
        let claims = check(&hs256(heyo_claims()), &heyo_spec()).await.unwrap();
        assert_eq!(claims.string("userId").as_deref(), Some("u_1f2e"));
        assert_eq!(claims.string("email").as_deref(), Some("someone@example.com"));
        assert_eq!(claims.string("role").as_deref(), Some("admin"));
    }

    #[tokio::test]
    async fn a_token_signed_with_another_secret_does_not_verify() {
        let forged = token(
            json!({"alg": "HS256"}),
            heyo_claims(),
            hmac_with(b"not-the-secret", MessageDigest::sha256()),
        );
        assert_eq!(check(&forged, &heyo_spec()).await.unwrap_err(), JwtError::Signature);
    }

    /// The original JWT footgun: a header claiming the token needs no signature.
    /// There is no `none` arm to reach, so it is refused as an algorithm the
    /// gate does not accept — before anything looks at a signature.
    #[tokio::test]
    async fn alg_none_is_not_an_algorithm() {
        let unsigned = format!(
            "{}.{}.",
            b64(json!({"alg": "none"}).to_string().as_bytes()),
            b64(heyo_claims().to_string().as_bytes())
        );
        assert_eq!(
            check(&unsigned, &heyo_spec()).await.unwrap_err(),
            JwtError::Algorithm("none".into()),
        );
        assert!(Algorithm::parse("none").is_none());
        assert!(Algorithm::parse("None").is_none());
        assert!(Algorithm::parse("").is_none());
    }

    /// Algorithm confusion, the attack the `algorithms` list exists for: a gate
    /// holding an RSA *public* key, handed a token signed HS256 with that public
    /// key's bytes as the HMAC secret. The key is public, so anyone can produce
    /// this — and a verifier that dispatched on the header would accept it.
    #[tokio::test]
    async fn a_public_key_cannot_be_used_as_an_hmac_secret() {
        let rsa = openssl::rsa::Rsa::generate(2048).unwrap();
        let pem = PKey::from_rsa(rsa.clone()).unwrap().public_key_to_pem().unwrap();
        let public = public_key_from_pem(std::str::from_utf8(&pem).unwrap()).unwrap();

        let spec: JwtSpec = serde_json::from_value(json!({
            "public_key": String::from_utf8(pem.clone()).unwrap(),
            "algorithms": ["RS256", "HS256"],
            "issuer": "auth-service",
            "subject_claim": "userId",
        }))
        .unwrap();

        // The forger signs with the PEM bytes, which is what the naive attack
        // does — the "secret" is a value they can read off the wire.
        let forged = token(
            json!({"alg": "HS256"}),
            heyo_claims(),
            hmac_with(&pem, MessageDigest::sha256()),
        );

        let err = verify(&forged, &spec, Some(&Key::Public(public)), &JwksCache::new(), NOW)
            .await
            .unwrap_err();
        assert_eq!(err, JwtError::KeyMismatch, "an HS token verified against a public key");

        // ...and the same spec is refused at registration, so it cannot be
        // written down in the first place. Both checks, deliberately.
        let gate = json!({
            "provider": "jwt",
            "base_path": "/__applb/auth",
            "jwt": {
                "public_key": String::from_utf8(pem).unwrap(),
                "algorithms": ["RS256", "HS256"],
                "issuer": "auth-service",
            }
        });
        let gate: crate::config::AuthGate = serde_json::from_value(gate).unwrap();
        assert!(matches!(
            gate.jwt.as_ref().unwrap().validate_for_test(),
            Err(crate::config::SpecError::JwtAlgorithmKeyMismatch { .. }),
        ));
    }

    /// A correctly-signed token in an algorithm the gate did not name. Stronger
    /// than it looks: with one secret, HS256 and HS512 are both computable by
    /// whoever holds it, so this is what pins a gate to the algorithm its issuer
    /// actually uses.
    #[tokio::test]
    async fn an_algorithm_the_gate_did_not_name_is_refused() {
        let hs512 = token(
            json!({"alg": "HS512"}),
            heyo_claims(),
            hmac_with(SECRET, MessageDigest::sha512()),
        );
        assert_eq!(
            check(&hs512, &heyo_spec()).await.unwrap_err(),
            JwtError::Algorithm("HS512".into()),
        );

        // Named, and it verifies.
        let mut spec = heyo_spec();
        spec.algorithms = vec!["HS512".into()];
        check(&hs512, &spec).await.unwrap();
    }

    #[tokio::test]
    async fn expiry_is_required_and_enforced() {
        let mut claims = heyo_claims();
        claims.as_object_mut().unwrap().remove("exp");
        assert!(matches!(
            check(&hs256(claims), &heyo_spec()).await.unwrap_err(),
            JwtError::Expiry(_),
        ));

        let mut claims = heyo_claims();
        claims["exp"] = json!(NOW - 1);
        assert_eq!(
            check(&hs256(claims), &heyo_spec()).await.unwrap_err(),
            JwtError::Expiry("the token has expired"),
        );

        // Leeway covers clock skew, and only as much as the spec allows.
        let mut claims = heyo_claims();
        claims["exp"] = json!(NOW - 20);
        let mut lenient = heyo_spec();
        lenient.leeway_secs = Some(60);
        check(&hs256(claims.clone()), &lenient).await.unwrap();
        lenient.leeway_secs = Some(5);
        assert!(check(&hs256(claims), &lenient).await.is_err());
    }

    #[tokio::test]
    async fn a_token_that_is_not_valid_yet_is_refused() {
        let mut claims = heyo_claims();
        claims["nbf"] = json!(NOW + 120);
        assert_eq!(
            check(&hs256(claims), &heyo_spec()).await.unwrap_err(),
            JwtError::Expiry("the token is not valid yet"),
        );
    }

    /// The issuer check is what a signature alone cannot give: with a shared
    /// secret, every service holding it can mint a token that verifies.
    #[tokio::test]
    async fn a_token_from_another_issuer_is_refused() {
        let mut claims = heyo_claims();
        claims["iss"] = json!("some-other-service");
        assert!(matches!(
            check(&hs256(claims), &heyo_spec()).await.unwrap_err(),
            JwtError::Claim(_),
        ));

        let mut claims = heyo_claims();
        claims.as_object_mut().unwrap().remove("iss");
        assert!(matches!(
            check(&hs256(claims), &heyo_spec()).await.unwrap_err(),
            JwtError::Claim(_),
        ));
    }

    /// `aud` is a string or an array of them, and both spellings are in the wild
    /// for the same issuer.
    #[tokio::test]
    async fn an_audience_matches_either_spelling() {
        let mut claims = heyo_claims();
        claims["aud"] = json!(["heyo-server", "heyo-app"]);
        check(&hs256(claims), &heyo_spec()).await.unwrap();

        let mut claims = heyo_claims();
        claims["aud"] = json!(["heyo-server"]);
        assert!(matches!(
            check(&hs256(claims), &heyo_spec()).await.unwrap_err(),
            JwtError::Claim(_),
        ));

        // A gate that names no audience does not check one.
        let mut spec = heyo_spec();
        spec.audience = None;
        let mut claims = heyo_claims();
        claims.as_object_mut().unwrap().remove("aud");
        check(&hs256(claims), &spec).await.unwrap();
    }

    /// The gate's allow-list: OR within a claim, AND across claims, and a claim
    /// that is itself a list is satisfied by containment.
    #[tokio::test]
    async fn require_is_or_within_a_claim_and_and_across_them() {
        let mut spec = heyo_spec();
        spec.require = [
            ("role".to_string(), json!(["admin", "owner"])),
            ("accountId".to_string(), json!("acct_7f3c")),
        ]
        .into_iter()
        .collect();
        check(&hs256(heyo_claims()), &spec).await.unwrap();

        // One of the two fails ⇒ refused.
        let mut claims = heyo_claims();
        claims["accountId"] = json!("acct_other");
        assert_eq!(
            check(&hs256(claims), &spec).await.unwrap_err(),
            JwtError::Require("accountId".into()),
        );

        // A missing claim is a failed requirement, not an absent one.
        let mut claims = heyo_claims();
        claims.as_object_mut().unwrap().remove("role");
        assert_eq!(
            check(&hs256(claims), &spec).await.unwrap_err(),
            JwtError::Require("role".into()),
        );

        // A list-valued claim — every scope claim in existence — is satisfied by
        // holding one of the wanted values.
        let mut spec = heyo_spec();
        spec.require = [("scopes".to_string(), json!("deploy"))].into_iter().collect();
        let mut claims = heyo_claims();
        claims["scopes"] = json!(["read", "deploy", "write"]);
        check(&hs256(claims.clone()), &spec).await.unwrap();
        claims["scopes"] = json!(["read"]);
        assert!(check(&hs256(claims), &spec).await.is_err());
    }

    /// Numbers and booleans are compared as themselves, so `{"tier": 2}` and
    /// `{"verified": true}` are expressible without quoting them into strings.
    #[tokio::test]
    async fn require_compares_non_string_claims() {
        let mut spec = heyo_spec();
        spec.require = [
            ("tier".to_string(), json!(2)),
            ("verified".to_string(), json!(true)),
        ]
        .into_iter()
        .collect();
        let mut claims = heyo_claims();
        claims["tier"] = json!(2);
        claims["verified"] = json!(true);
        check(&hs256(claims.clone()), &spec).await.unwrap();

        // A string "2" is not the number 2, and saying so beats guessing.
        claims["tier"] = json!("2");
        assert!(check(&hs256(claims), &spec).await.is_err());
    }

    #[tokio::test]
    async fn a_malformed_token_is_refused_before_anything_else() {
        for bad in [
            "",
            "onlyonesegment",
            "two.segments",
            "four.seg.ments.here",
            "!!!.###.$$$",
        ] {
            assert!(
                check(bad, &heyo_spec()).await.is_err(),
                "{bad:?} was accepted",
            );
        }
    }

    // -- asymmetric ---------------------------------------------------------

    fn rsa_pair() -> (openssl::rsa::Rsa<openssl::pkey::Private>, String) {
        let rsa = openssl::rsa::Rsa::generate(2048).unwrap();
        let pem = PKey::from_rsa(rsa.clone()).unwrap().public_key_to_pem().unwrap();
        (rsa, String::from_utf8(pem).unwrap())
    }

    #[tokio::test]
    async fn rs256_and_ps256_verify_against_a_pem() {
        let (rsa, pem) = rsa_pair();
        let private = PKey::from_rsa(rsa).unwrap();
        let public = public_key_from_pem(&pem).unwrap();

        for (alg, padding) in [
            ("RS256", openssl::rsa::Padding::PKCS1),
            ("PS256", openssl::rsa::Padding::PKCS1_PSS),
        ] {
            let signed = token(json!({"alg": alg}), heyo_claims(), |data| {
                let mut signer =
                    openssl::sign::Signer::new(MessageDigest::sha256(), &private).unwrap();
                signer.set_rsa_padding(padding).unwrap();
                if padding == openssl::rsa::Padding::PKCS1_PSS {
                    signer.set_rsa_mgf1_md(MessageDigest::sha256()).unwrap();
                    signer
                        .set_rsa_pss_saltlen(openssl::sign::RsaPssSaltlen::DIGEST_LENGTH)
                        .unwrap();
                }
                signer.update(data).unwrap();
                signer.sign_to_vec().unwrap()
            });

            let mut spec = heyo_spec();
            spec.secret = None;
            spec.public_key = Some(pem.clone());
            spec.algorithms = vec![alg.to_string()];

            let public = public_key_from_pem(&pem).unwrap();
            verify(&signed, &spec, Some(&Key::Public(public)), &JwksCache::new(), NOW)
                .await
                .unwrap_or_else(|e| panic!("{alg} did not verify: {e}"));
        }

        // A token signed by a different key of the same kind.
        let (other, _) = rsa_pair();
        let other = PKey::from_rsa(other).unwrap();
        let forged = token(json!({"alg": "RS256"}), heyo_claims(), |data| {
            let mut signer = openssl::sign::Signer::new(MessageDigest::sha256(), &other).unwrap();
            signer.update(data).unwrap();
            signer.sign_to_vec().unwrap()
        });
        let mut spec = heyo_spec();
        spec.secret = None;
        spec.public_key = Some(pem);
        spec.algorithms = vec!["RS256".into()];
        assert_eq!(
            verify(&forged, &spec, Some(&Key::Public(public)), &JwksCache::new(), NOW)
                .await
                .unwrap_err(),
            JwtError::Signature,
        );
    }

    /// ES256's signature is raw `r‖s`, not the DER OpenSSL verifies. Getting
    /// that conversion wrong fails *every* ES token, which is why it has a test
    /// of its own rather than riding along with the RSA one.
    #[tokio::test]
    async fn es256_raw_signatures_are_converted_before_verifying() {
        let group =
            openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1).unwrap();
        let key = openssl::ec::EcKey::generate(&group).unwrap();
        let public_pem = PKey::from_ec_key(key.clone()).unwrap().public_key_to_pem().unwrap();
        let public_pem = String::from_utf8(public_pem).unwrap();

        let signed = token(json!({"alg": "ES256"}), heyo_claims(), |data| {
            let digest = openssl::hash::hash(MessageDigest::sha256(), data).unwrap();
            let sig = openssl::ecdsa::EcdsaSig::sign(&digest, &key).unwrap();
            // JWS carries the two halves at fixed width, left-padded.
            let mut raw = vec![0u8; 64];
            let (r, s) = (sig.r().to_vec(), sig.s().to_vec());
            raw[32 - r.len()..32].copy_from_slice(&r);
            raw[64 - s.len()..].copy_from_slice(&s);
            raw
        });

        let mut spec = heyo_spec();
        spec.secret = None;
        spec.public_key = Some(public_pem.clone());
        spec.algorithms = vec!["ES256".into()];
        let public = public_key_from_pem(&public_pem).unwrap();
        verify(&signed, &spec, Some(&Key::Public(public)), &JwksCache::new(), NOW)
            .await
            .unwrap();

        // A signature of the wrong width is refused rather than misread.
        let truncated = {
            let (body, sig) = signed.rsplit_once('.').unwrap();
            let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(sig).unwrap();
            format!("{body}.{}", b64(&raw[..48]))
        };
        let public = public_key_from_pem(&public_pem).unwrap();
        assert_eq!(
            verify(&truncated, &spec, Some(&Key::Public(public)), &JwksCache::new(), NOW)
                .await
                .unwrap_err(),
            JwtError::Signature,
        );
    }

    #[test]
    fn a_pem_is_accepted_bare_or_wrapped_in_a_certificate() {
        let (_, pem) = rsa_pair();
        public_key_from_pem(&pem).unwrap();
        assert!(public_key_from_pem("not a key at all").is_err());
        assert!(public_key_from_pem("").is_err());
    }

    // -- JWKS ---------------------------------------------------------------

    /// Build a JWKS document from a generated RSA key, the way an issuer
    /// publishes one.
    fn rsa_jwks(kid: &str) -> (openssl::rsa::Rsa<openssl::pkey::Private>, String) {
        let rsa = openssl::rsa::Rsa::generate(2048).unwrap();
        let doc = json!({
            "keys": [{
                "kty": "RSA",
                "kid": kid,
                "alg": "RS256",
                "use": "sig",
                "n": b64(&rsa.n().to_vec()),
                "e": b64(&rsa.e().to_vec()),
            }]
        });
        (rsa, doc.to_string())
    }

    #[tokio::test]
    async fn a_key_set_verifies_the_key_its_kid_names() {
        let (rsa, document) = rsa_jwks("key-2026-08");
        let private = PKey::from_rsa(rsa).unwrap();
        let jwks = JwksCache::new();
        jwks.seed("https://idp.example.com/jwks", &document, NOW).await;

        let signed = token(
            json!({"alg": "RS256", "kid": "key-2026-08"}),
            heyo_claims(),
            |data| {
                let mut signer =
                    openssl::sign::Signer::new(MessageDigest::sha256(), &private).unwrap();
                signer.update(data).unwrap();
                signer.sign_to_vec().unwrap()
            },
        );

        let mut spec = heyo_spec();
        spec.secret = None;
        spec.jwks_url = Some("https://idp.example.com/jwks".into());
        spec.algorithms = vec!["RS256".into()];

        verify(&signed, &spec, None, &jwks, NOW).await.unwrap();

        // A kid the set does not hold. The refetch it would trigger is
        // rate-limited, so within the window this is answered from cache — which
        // is the point: an unknown kid is attacker-controlled.
        let unknown = token(
            json!({"alg": "RS256", "kid": "not-a-key"}),
            heyo_claims(),
            |data| {
                let mut signer =
                    openssl::sign::Signer::new(MessageDigest::sha256(), &private).unwrap();
                signer.update(data).unwrap();
                signer.sign_to_vec().unwrap()
            },
        );
        assert_eq!(
            verify(&unknown, &spec, None, &jwks, NOW).await.unwrap_err(),
            JwtError::UnknownKey("not-a-key".into()),
        );
    }

    /// A token naming no `kid` takes the set's only key — and is refused when
    /// there is a choice, because "it verified under one of them" is a weaker
    /// statement than it looks.
    #[tokio::test]
    async fn an_unlabelled_token_is_only_served_an_unambiguous_key_set() {
        let (rsa, one) = rsa_jwks("only");
        let jwks = JwksCache::new();
        jwks.seed("https://idp.example.com/one", &one, NOW).await;
        assert!(jwks.key_for("https://idp.example.com/one", None, NOW).await.is_ok());

        let mut doc: serde_json::Value = serde_json::from_str(&one).unwrap();
        let (second, _) = rsa_jwks("second");
        doc["keys"].as_array_mut().unwrap().push(json!({
            "kty": "RSA",
            "kid": "second",
            "n": b64(&second.n().to_vec()),
            "e": b64(&second.e().to_vec()),
        }));
        jwks.seed("https://idp.example.com/two", &doc.to_string(), NOW).await;
        assert!(matches!(
            jwks.key_for("https://idp.example.com/two", None, NOW).await,
            Err(JwtError::UnknownKey(_)),
        ));
        let _ = rsa;
    }

    #[tokio::test]
    async fn an_ec_jwk_becomes_a_verifying_key() {
        let group =
            openssl::ec::EcGroup::from_curve_name(openssl::nid::Nid::X9_62_PRIME256V1).unwrap();
        let key = openssl::ec::EcKey::generate(&group).unwrap();
        let mut ctx = openssl::bn::BigNumContext::new().unwrap();
        let (mut x, mut y) = (openssl::bn::BigNum::new().unwrap(), openssl::bn::BigNum::new().unwrap());
        key.public_key()
            .affine_coordinates_gfp(&group, &mut x, &mut y, &mut ctx)
            .unwrap();

        let document = json!({
            "keys": [{"kty": "EC", "kid": "ec", "crv": "P-256", "x": b64(&x.to_vec()), "y": b64(&y.to_vec())}]
        })
        .to_string();
        let jwks = JwksCache::new();
        jwks.seed("https://idp.example.com/ec", &document, NOW).await;
        jwks.key_for("https://idp.example.com/ec", Some("ec"), NOW)
            .await
            .expect("an EC JWK must become a key");

        // A curve nothing here can verify is named rather than guessed at.
        let document = json!({
            "keys": [{"kty": "EC", "kid": "ec", "crv": "P-521", "x": b64(&x.to_vec()), "y": b64(&y.to_vec())}]
        })
        .to_string();
        jwks.seed("https://idp.example.com/p521", &document, NOW).await;
        assert!(matches!(
            jwks.key_for("https://idp.example.com/p521", Some("ec"), NOW).await,
            Err(JwtError::Key(_)),
        ));
    }

    /// A key set that cannot be reached and was never cached fails the request;
    /// one that cannot be *refreshed* keeps serving what it has, because an
    /// issuer's key endpoint blipping must not take the deployment down.
    #[tokio::test]
    async fn an_unreachable_key_set_is_only_fatal_when_nothing_is_cached() {
        let jwks = JwksCache::new();
        // Port 1: nothing listens, and the connection is refused immediately.
        let url = "http://127.0.0.1:1/jwks";
        assert!(matches!(
            jwks.key_for(url, Some("k"), NOW).await,
            Err(JwtError::Jwks(_)),
        ));

        let (_, document) = rsa_jwks("cached");
        jwks.seed(url, &document, NOW).await;
        // An hour later the set is stale, the refresh fails, and the cached key
        // is still served.
        jwks.key_for(url, Some("cached"), NOW + 3600)
            .await
            .expect("a cached key outlives a failed refresh");
    }
}
