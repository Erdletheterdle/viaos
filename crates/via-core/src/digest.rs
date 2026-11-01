//! Content-addressed digests.
//!
//! Via identifies every immutable artifact (image layers, image configs,
//! bundle members) by digest. The string form is the OCI form, `sha256:<hex>`,
//! so digests are directly comparable with values reported by Docker.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::Error;

/// A content digest in OCI string form, e.g. `sha256:e3b0c442...`.
///
/// Construction is always validated, so a `Digest` is guaranteed to be
/// well-formed and lowercase.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Digest(String);

impl Digest {
    /// Compute the SHA-256 digest of `bytes`.
    pub fn sha256_of(bytes: impl AsRef<[u8]>) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes.as_ref());
        Self(format!("sha256:{}", hex::encode(hasher.finalize())))
    }

    /// Parse and validate a digest string.
    pub fn parse(s: impl Into<String>) -> Result<Self, Error> {
        let s = s.into();
        let (algo, hex) = s
            .split_once(':')
            .ok_or_else(|| Error::invalid_digest(&s, "missing ':' separator"))?;

        let expected_len = match algo {
            "sha256" => 64,
            "sha512" => 128,
            other => {
                return Err(Error::invalid_digest(
                    &s,
                    format!("unsupported algorithm {other:?}"),
                ));
            }
        };

        if hex.len() != expected_len {
            return Err(Error::invalid_digest(
                &s,
                format!(
                    "expected {expected_len} hex chars for {algo}, found {}",
                    hex.len()
                ),
            ));
        }
        if !hex
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(Error::invalid_digest(&s, "digest must be lowercase hex"));
        }
        Ok(Self(s))
    }

    /// The algorithm portion, e.g. `sha256`.
    pub fn algorithm(&self) -> &str {
        self.0.split_once(':').map(|(a, _)| a).unwrap_or("")
    }

    /// The hex portion, without the algorithm prefix.
    pub fn hex(&self) -> &str {
        self.0.split_once(':').map(|(_, h)| h).unwrap_or("")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Filesystem-safe form, `sha256-<hex>`, used for blob paths inside a bundle.
    ///
    /// `:` is legal on Linux but hostile on other platforms and in URLs, and
    /// bundles are meant to be copied around by ordinary means (USB, scp, S3).
    pub fn to_path_segment(&self) -> String {
        self.0.replace(':', "-")
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.0)
    }
}

impl FromStr for Digest {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Digest {
    type Error = Error;
    fn try_from(s: String) -> Result<Self, Error> {
        Self::parse(s)
    }
}

impl From<Digest> for String {
    fn from(d: Digest) -> String {
        d.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_sha256_matches_known_value() {
        assert_eq!(
            Digest::sha256_of(b"").as_str(),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn rejects_malformed() {
        assert!(Digest::parse("e3b0c442").is_err(), "missing algorithm");
        assert!(Digest::parse("sha256:abc").is_err(), "wrong length");
        assert!(Digest::parse("md5:abc").is_err(), "unsupported algorithm");
        let upper = format!("sha256:{}", "A".repeat(64));
        assert!(Digest::parse(upper).is_err(), "uppercase hex");
    }

    #[test]
    fn path_segment_is_filesystem_safe() {
        let d = Digest::sha256_of(b"hello");
        assert!(!d.to_path_segment().contains(':'));
        assert!(d.to_path_segment().starts_with("sha256-"));
    }
}
