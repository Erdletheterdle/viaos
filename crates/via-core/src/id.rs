//! Validated identifiers for the product object model (spec section 5).
//!
//! These are newtypes rather than `String` because they end up in filesystem
//! paths, Docker object names, and URLs, where an unvalidated string is a bug
//! waiting to happen.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Error;

/// Validate a DNS-label-ish slug: lowercase alphanumeric plus `-` and `_`,
/// starting and ending alphanumeric. This is the intersection of what Docker
/// accepts for object names and what is safe in a path or URL.
fn validate_slug(kind: &'static str, s: &str) -> Result<(), Error> {
    if s.is_empty() {
        return Err(Error::InvalidId {
            kind,
            value: s.into(),
            reason: "must not be empty".into(),
        });
    }
    if s.len() > 63 {
        return Err(Error::InvalidId {
            kind,
            value: s.into(),
            reason: "must be 63 characters or fewer".into(),
        });
    }
    let ok_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_';
    if let Some(bad) = s.chars().find(|c| !ok_char(*c)) {
        return Err(Error::InvalidId {
            kind,
            value: s.into(),
            reason: format!("illegal character {bad:?}; use lowercase letters, digits, '-' or '_'"),
        });
    }
    let boundary_ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    if !s.starts_with(boundary_ok) || !s.ends_with(boundary_ok) {
        return Err(Error::InvalidId {
            kind,
            value: s.into(),
            reason: "must start and end with a letter or digit".into(),
        });
    }
    Ok(())
}

macro_rules! slug_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn parse(s: impl Into<String>) -> Result<Self, Error> {
                let s = s.into();
                validate_slug($kind, &s)?;
                Ok(Self(s))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", $kind, self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(s: &str) -> Result<Self, Error> {
                Self::parse(s)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;
            fn try_from(s: String) -> Result<Self, Error> {
                Self::parse(s)
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.0
            }
        }
    };
}

slug_id!(
    /// Name of an Application, e.g. `demo-stack`. Doubles as the Compose
    /// project name, which is what keeps volumes stable across releases
    /// (spec section 19).
    AppName, "AppName"
);

slug_id!(
    /// Name of a single service within an Application, e.g. `detector`.
    ServiceName, "ServiceName"
);

slug_id!(
    /// Stable identity of one enrolled device (spec section 31).
    DeviceId, "DeviceId"
);

/// A monotonically increasing release number within an Application
/// (spec section 8). Releases are numbered, not named, so ordering and
/// rollback targets are unambiguous.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReleaseId(pub u64);

impl fmt::Display for ReleaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for ReleaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Release({})", self.0)
    }
}

impl FromStr for ReleaseId {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self, Error> {
        s.trim_start_matches('#')
            .parse::<u64>()
            .map(ReleaseId)
            .map_err(|e| Error::InvalidId {
                kind: "ReleaseId",
                value: s.into(),
                reason: e.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        assert!(AppName::parse("demo-stack").is_ok());
        assert!(AppName::parse("node01").is_ok());
        assert!(ServiceName::parse("postgres").is_ok());
    }

    #[test]
    fn rejects_names_that_would_break_paths_or_docker() {
        assert!(AppName::parse("").is_err());
        assert!(AppName::parse("Demo-Stack").is_err(), "uppercase");
        assert!(AppName::parse("demo stack").is_err(), "space");
        assert!(AppName::parse("../etc").is_err(), "traversal");
        assert!(AppName::parse("-leading").is_err());
        assert!(AppName::parse("trailing-").is_err());
        assert!(AppName::parse("a".repeat(64)).is_err(), "too long");
    }

    #[test]
    fn release_id_parses_with_or_without_hash() {
        assert_eq!("44".parse::<ReleaseId>().unwrap(), ReleaseId(44));
        assert_eq!("#44".parse::<ReleaseId>().unwrap(), ReleaseId(44));
        assert!("beta".parse::<ReleaseId>().is_err());
    }
}
