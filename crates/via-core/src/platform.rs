//! Target platform identity.
//!
//! Bundles are architecture-specific (spec section 12), so platform is a
//! first-class part of a release artifact's identity rather than a tag suffix.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Error;

/// An OCI-style platform, e.g. `linux/arm64` or `linux/arm/v7`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Platform {
    pub os: String,
    pub arch: String,
    pub variant: Option<String>,
}

impl Platform {
    pub fn new(os: impl Into<String>, arch: impl Into<String>) -> Self {
        Self {
            os: os.into(),
            arch: arch.into(),
            variant: None,
        }
    }

    pub fn with_variant(mut self, variant: impl Into<String>) -> Self {
        self.variant = Some(variant.into());
        self
    }

    /// The platform of the machine this code is running on.
    pub fn host() -> Self {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "arm" => "arm",
            other => other,
        };
        Self::new(std::env::consts::OS, arch)
    }

    pub fn parse(s: &str) -> Result<Self, Error> {
        let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
        match parts.as_slice() {
            [os, arch] => Ok(Self::new(*os, *arch)),
            [os, arch, variant] => Ok(Self::new(*os, *arch).with_variant(*variant)),
            _ => Err(Error::InvalidPlatform {
                value: s.to_string(),
                reason: "expected <os>/<arch> or <os>/<arch>/<variant>".into(),
            }),
        }
    }

    /// Filesystem-safe form used in bundle filenames, e.g. `linux-arm64`.
    pub fn to_file_segment(&self) -> String {
        match &self.variant {
            Some(v) => format!("{}-{}-{}", self.os, self.arch, v),
            None => format!("{}-{}", self.os, self.arch),
        }
    }

    /// Whether an image built for `self` can run on `host`.
    ///
    /// Deliberately strict: an exact match, or an unspecified variant matching
    /// a specific one. Via would rather fail a deploy than silently run an
    /// image the device cannot execute (spec section 11).
    pub fn is_compatible_with(&self, host: &Platform) -> bool {
        self.os == host.os
            && self.arch == host.arch
            && match (&self.variant, &host.variant) {
                (None, _) | (_, None) => true,
                (Some(a), Some(b)) => a == b,
            }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.variant {
            Some(v) => write!(f, "{}/{}/{}", self.os, self.arch, v),
            None => write!(f, "{}/{}", self.os, self.arch),
        }
    }
}

impl fmt::Debug for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Platform({self})")
    }
}

impl FromStr for Platform {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        Self::parse(s)
    }
}

impl TryFrom<String> for Platform {
    type Error = Error;
    fn try_from(s: String) -> Result<Self, Error> {
        Self::parse(&s)
    }
}

impl From<Platform> for String {
    fn from(p: Platform) -> String {
        p.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_and_three_part_forms() {
        assert_eq!(
            Platform::parse("linux/arm64").unwrap().to_string(),
            "linux/arm64"
        );
        assert_eq!(
            Platform::parse("linux/arm/v7").unwrap().to_string(),
            "linux/arm/v7"
        );
        assert!(Platform::parse("linux").is_err());
    }

    #[test]
    fn compatibility_is_strict_on_arch() {
        let arm = Platform::parse("linux/arm64").unwrap();
        let amd = Platform::parse("linux/amd64").unwrap();
        assert!(arm.is_compatible_with(&arm));
        assert!(!arm.is_compatible_with(&amd));
    }

    #[test]
    fn file_segment_has_no_slashes() {
        assert_eq!(
            Platform::parse("linux/arm/v7").unwrap().to_file_segment(),
            "linux-arm-v7"
        );
    }
}
