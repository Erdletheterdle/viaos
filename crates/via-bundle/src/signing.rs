//! Bundle signatures.
//!
//! Via signs the **manifest**, not the whole archive file. The manifest is the
//! first entry in the bundle and already carries the digest of every blob and
//! of the deployment, so a signature over those few hundred bytes covers the
//! entire release transitively.
//!
//! That choice buys one thing that matters on a slow link: a device can verify
//! authenticity after the first kilobyte rather than after downloading a
//! gigabyte and hashing it. A forged or tampered bundle is rejected before its
//! payload is transferred at all.
//!
//! ```text
//! via-bundle.json   manifest        <- signed
//! via-bundle.sig    signature       <- verified against a pinned public key
//! deployment.json                   <- covered: manifest holds its digest
//! blobs/sha256-...                  <- covered: manifest holds every digest
//! ```

use std::fmt;
use std::path::Path;

use ed25519_dalek::{Signer, Verifier};
use rand::TryRng;
use serde::{Deserialize, Serialize};
use via_core::Digest;

use crate::Error;

/// Path of the signature inside the bundle archive.
pub const SIGNATURE_PATH: &str = "via-bundle.sig";

/// The only algorithm Via signs with.
///
/// Recorded explicitly so a future algorithm can be added without a verifier
/// silently accepting something it does not understand.
pub const ALGORITHM_ED25519: &str = "ed25519";

/// A detached signature over the manifest bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleSignature {
    pub algorithm: String,
    /// Identifies which key signed, so a verifier holding several can pick the
    /// right one and report a useful error when it holds none of them.
    pub key_id: KeyId,
    /// Hex-encoded signature over the raw manifest bytes.
    pub signature: String,
}

impl BundleSignature {
    fn signature_bytes(&self) -> Result<[u8; 64], Error> {
        let raw = hex::decode(&self.signature).map_err(|_| Error::MalformedSignature {
            reason: "signature is not valid hex".into(),
        })?;
        raw.try_into().map_err(|_| Error::MalformedSignature {
            reason: "signature must be 64 bytes".into(),
        })
    }
}

/// Short, stable identifier for a public key: the first 16 hex characters of
/// its SHA-256 digest.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyId(String);

impl KeyId {
    fn of(public: &[u8; 32]) -> Self {
        let digest = Digest::sha256_of(public);
        Self(digest.hex()[..16].to_string())
    }

    /// Parse a previously recorded key id.
    ///
    /// A key id is a label, not a secret and not a capability: it selects
    /// which trusted key to check against, and the signature check is what
    /// actually decides anything. Validation here is only to keep malformed
    /// values out of error messages and log lines.
    pub fn parse(s: impl Into<String>) -> Result<Self, Error> {
        let s = s.into();
        if s.len() != 16
            || !s
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Error::MalformedKey {
                kind: "key id",
                reason: "expected 16 lowercase hex characters".into(),
            });
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for KeyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "KeyId({})", self.0)
    }
}

/// A private signing key. Lives on a build machine or in CI, never on a device.
pub struct SigningKey(ed25519_dalek::SigningKey);

impl SigningKey {
    /// Generate a key from operating-system entropy.
    ///
    /// Seeds from the OS directly rather than through a generic RNG trait, so
    /// this does not break when `rand` and `ed25519-dalek` disagree about
    /// which `rand_core` version they speak.
    pub fn generate() -> Self {
        let mut secret = [0u8; 32];
        rand::rngs::SysRng
            .try_fill_bytes(&mut secret)
            .expect("the operating system must be able to provide entropy");
        Self(ed25519_dalek::SigningKey::from_bytes(&secret))
    }

    pub fn public(&self) -> PublicKey {
        PublicKey(self.0.verifying_key())
    }

    pub fn key_id(&self) -> KeyId {
        self.public().key_id()
    }

    /// Hex-encoded private key material.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0.to_bytes())
    }

    pub fn from_hex(s: &str) -> Result<Self, Error> {
        let raw = decode_key_hex(s, "private")?;
        Ok(Self(ed25519_dalek::SigningKey::from_bytes(&raw)))
    }

    /// Read a key from a file, tolerating comment lines and surrounding
    /// whitespace so a key file can carry a human-readable header.
    pub fn read_from(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let body = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Self::from_hex(&strip_comments(&body))
    }

    pub fn sign_manifest(&self, manifest_bytes: &[u8]) -> BundleSignature {
        let signature = self.0.sign(manifest_bytes);
        BundleSignature {
            algorithm: ALGORITHM_ED25519.to_string(),
            key_id: self.key_id(),
            signature: hex::encode(signature.to_bytes()),
        }
    }
}

/// A public key a device trusts.
#[derive(Clone)]
pub struct PublicKey(ed25519_dalek::VerifyingKey);

impl PublicKey {
    pub fn key_id(&self) -> KeyId {
        KeyId::of(&self.0.to_bytes())
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0.to_bytes())
    }

    pub fn from_hex(s: &str) -> Result<Self, Error> {
        let raw = decode_key_hex(s, "public")?;
        let key =
            ed25519_dalek::VerifyingKey::from_bytes(&raw).map_err(|e| Error::MalformedKey {
                kind: "public",
                reason: e.to_string(),
            })?;
        Ok(Self(key))
    }

    pub fn read_from(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let body = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        Self::from_hex(&strip_comments(&body))
    }

    /// Read every key in a file, one per line, ignoring comments and blanks.
    ///
    /// Multiple keys in one file is what makes key rotation possible without a
    /// flag day: a device trusts the old and new key at once while a fleet
    /// migrates.
    pub fn read_all_from(path: impl AsRef<Path>) -> Result<Vec<PublicKey>, Error> {
        let path = path.as_ref();
        let body = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let mut keys = Vec::new();
        for line in body.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            keys.push(Self::from_hex(line)?);
        }
        Ok(keys)
    }
}

fn strip_comments(body: &str) -> String {
    body.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn decode_key_hex(s: &str, kind: &'static str) -> Result<[u8; 32], Error> {
    let s = s.trim();
    let raw = hex::decode(s).map_err(|_| Error::MalformedKey {
        kind,
        reason: "expected 64 hex characters".into(),
    })?;
    raw.try_into().map_err(|_| Error::MalformedKey {
        kind,
        reason: "key must be 32 bytes".into(),
    })
}

/// What a verifier requires of a bundle.
#[derive(Clone)]
pub enum TrustPolicy {
    /// Accept unsigned bundles and ignore any signature present.
    ///
    /// For local development, where the bundle never leaves the machine that
    /// built it. A device should never run with this.
    Insecure,
    /// Require a valid signature from one of these keys.
    ///
    /// An unsigned bundle, a signature from an unknown key, and a signature
    /// that does not verify are all rejected, and each reports a different
    /// error, because they mean very different things operationally.
    Require(Vec<PublicKey>),
}

impl TrustPolicy {
    pub fn from_keys(keys: Vec<PublicKey>) -> Self {
        if keys.is_empty() {
            TrustPolicy::Insecure
        } else {
            TrustPolicy::Require(keys)
        }
    }

    pub fn requires_signature(&self) -> bool {
        matches!(self, TrustPolicy::Require(_))
    }

    /// Check a manifest against this policy.
    pub fn verify(
        &self,
        manifest_bytes: &[u8],
        signature: Option<&BundleSignature>,
    ) -> Result<Verified, Error> {
        let TrustPolicy::Require(trusted) = self else {
            return Ok(match signature {
                Some(s) => Verified::UncheckedSignature {
                    key_id: s.key_id.clone(),
                },
                None => Verified::Unsigned,
            });
        };

        let Some(signature) = signature else {
            return Err(Error::BundleUnsigned);
        };
        if signature.algorithm != ALGORITHM_ED25519 {
            return Err(Error::UnsupportedSignatureAlgorithm {
                algorithm: signature.algorithm.clone(),
            });
        }

        let Some(key) = trusted.iter().find(|k| k.key_id() == signature.key_id) else {
            return Err(Error::UntrustedSigner {
                key_id: signature.key_id.clone(),
                trusted: trusted.iter().map(|k| k.key_id()).collect(),
            });
        };

        let bytes = signature.signature_bytes()?;
        let sig = ed25519_dalek::Signature::from_bytes(&bytes);
        key.0
            .verify(manifest_bytes, &sig)
            .map_err(|_| Error::SignatureVerificationFailed {
                key_id: signature.key_id.clone(),
            })?;

        Ok(Verified::Signed {
            key_id: signature.key_id.clone(),
        })
    }
}

/// The trust outcome for a bundle, carried alongside its contents so a caller
/// can report how the release was authenticated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verified {
    /// Signed and verified against a trusted key.
    Signed { key_id: KeyId },
    /// A signature was present but the policy did not check it.
    UncheckedSignature { key_id: KeyId },
    /// No signature, accepted because the policy did not require one.
    Unsigned,
}

impl Verified {
    pub fn is_verified(&self) -> bool {
        matches!(self, Verified::Signed { .. })
    }
}

impl fmt::Display for Verified {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Verified::Signed { key_id } => write!(f, "signed by {key_id}"),
            Verified::UncheckedSignature { key_id } => {
                write!(f, "signature by {key_id} (not checked)")
            }
            Verified::Unsigned => f.write_str("unsigned"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_verifies_against_its_own_key() {
        let key = SigningKey::generate();
        let manifest = br#"{"schema_version":1}"#;
        let sig = key.sign_manifest(manifest);

        let policy = TrustPolicy::Require(vec![key.public()]);
        assert_eq!(
            policy.verify(manifest, Some(&sig)).unwrap(),
            Verified::Signed {
                key_id: key.key_id()
            }
        );
    }

    #[test]
    fn tampering_with_the_manifest_breaks_the_signature() {
        // The manifest holds every blob digest, so this is what stops a
        // modified release from being accepted.
        let key = SigningKey::generate();
        let sig = key.sign_manifest(br#"{"release":44}"#);
        let policy = TrustPolicy::Require(vec![key.public()]);

        let err = policy.verify(br#"{"release":45}"#, Some(&sig)).unwrap_err();
        assert!(
            matches!(err, Error::SignatureVerificationFailed { .. }),
            "{err}"
        );
    }

    #[test]
    fn a_signature_from_an_unknown_key_is_refused_and_names_the_key() {
        let signer = SigningKey::generate();
        let other = SigningKey::generate();
        let manifest = b"payload";
        let sig = signer.sign_manifest(manifest);

        let policy = TrustPolicy::Require(vec![other.public()]);
        let err = policy.verify(manifest, Some(&sig)).unwrap_err();
        let msg = err.to_string();
        assert!(matches!(err, Error::UntrustedSigner { .. }), "{msg}");
        assert!(
            msg.contains(signer.key_id().as_str()),
            "the error should name the offending key: {msg}"
        );
    }

    #[test]
    fn an_unsigned_bundle_is_refused_when_signatures_are_required() {
        let key = SigningKey::generate();
        let policy = TrustPolicy::Require(vec![key.public()]);
        assert!(matches!(
            policy.verify(b"payload", None),
            Err(Error::BundleUnsigned)
        ));
    }

    #[test]
    fn insecure_policy_accepts_anything_but_says_so() {
        let policy = TrustPolicy::Insecure;
        assert_eq!(policy.verify(b"payload", None).unwrap(), Verified::Unsigned);

        let key = SigningKey::generate();
        let sig = key.sign_manifest(b"different payload");
        // Present but unchecked, and the result records that distinction so a
        // caller cannot mistake it for a verified bundle.
        let outcome = policy.verify(b"payload", Some(&sig)).unwrap();
        assert!(!outcome.is_verified());
    }

    #[test]
    fn keys_round_trip_through_hex() {
        let key = SigningKey::generate();
        let restored = SigningKey::from_hex(&key.to_hex()).unwrap();
        assert_eq!(restored.key_id(), key.key_id());

        let public = PublicKey::from_hex(&key.public().to_hex()).unwrap();
        assert_eq!(public.key_id(), key.key_id());
    }

    #[test]
    fn key_ids_are_derived_from_the_key_not_random() {
        let key = SigningKey::generate();
        assert_eq!(key.key_id(), key.public().key_id());
        assert_eq!(key.key_id().as_str().len(), 16);
    }

    #[test]
    fn key_ids_round_trip_through_their_string_form() {
        let key = SigningKey::generate();
        let id = key.key_id();
        assert_eq!(KeyId::parse(id.to_string()).unwrap(), id);
        assert!(KeyId::parse("nope").is_err());
        assert!(KeyId::parse("ABCDEF0123456789").is_err(), "uppercase");
    }

    #[test]
    fn malformed_key_material_is_rejected() {
        assert!(SigningKey::from_hex("not-hex").is_err());
        assert!(SigningKey::from_hex("aabb").is_err(), "wrong length");
        assert!(PublicKey::from_hex(&"00".repeat(31)).is_err());
    }

    #[test]
    fn an_empty_trusted_key_set_is_insecure_rather_than_impossible() {
        // Otherwise `Require(vec![])` would reject every bundle in existence
        // with a confusing message.
        assert!(!TrustPolicy::from_keys(Vec::new()).requires_signature());
        assert!(TrustPolicy::from_keys(vec![SigningKey::generate().public()]).requires_signature());
    }
}
