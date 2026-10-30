//! Release identity (spec sections 8, 9).

use serde::{Deserialize, Serialize};

use crate::{AppName, Platform, ReleaseId};

/// Points at one release of one application, for one platform.
///
/// A release is one logical version that may carry artifacts for several
/// architectures, so the platform is part of the *artifact* coordinate rather
/// than the release identity itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseRef {
    pub app: AppName,
    pub release: ReleaseId,
}

impl ReleaseRef {
    pub fn new(app: AppName, release: ReleaseId) -> Self {
        Self { app, release }
    }

    pub fn artifact(&self, platform: Platform) -> ArtifactRef {
        ArtifactRef { app: self.app.clone(), release: self.release, platform }
    }
}

impl std::fmt::Display for ReleaseRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.app, self.release)
    }
}

/// One concrete downloadable bundle: an application release built for a
/// specific platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub app: AppName,
    pub release: ReleaseId,
    pub platform: Platform,
}

impl ArtifactRef {
    /// Conventional bundle filename, e.g. `demo-stack-44-linux-arm64.bundle.zst`.
    pub fn file_name(&self) -> String {
        format!("{}-{}-{}.bundle.zst", self.app, self.release, self.platform.to_file_segment())
    }

    /// Control-plane storage path (spec section 62).
    pub fn store_path(&self) -> String {
        format!(
            "applications/{}/releases/{}/{}.bundle.zst",
            self.app,
            self.release,
            self.platform.to_file_segment()
        )
    }
}

impl std::fmt::Display for ArtifactRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{} ({})", self.app, self.release, self.platform)
    }
}

/// What release a device should be running (spec section 9).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReleaseTarget {
    /// Track whatever the application's current release is.
    Default,
    /// Held at a specific release, ignoring the application default. Used for
    /// test rigs and for devices an operator has deliberately frozen.
    Pinned { release: ReleaseId },
}

impl ReleaseTarget {
    /// Resolve to a concrete release given the application's current default.
    ///
    /// Returns `None` only when a device tracks the default and no default has
    /// been set yet, which is the state of a freshly enrolled device.
    pub fn resolve(&self, app_default: Option<ReleaseId>) -> Option<ReleaseId> {
        match self {
            ReleaseTarget::Default => app_default,
            ReleaseTarget::Pinned { release } => Some(*release),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> AppName {
        AppName::parse("demo-stack").unwrap()
    }

    #[test]
    fn artifact_file_name_is_conventional() {
        let a = ReleaseRef::new(app(), ReleaseId(44)).artifact(Platform::new("linux", "arm64"));
        assert_eq!(a.file_name(), "demo-stack-44-linux-arm64.bundle.zst");
    }

    #[test]
    fn store_path_matches_spec_layout() {
        let a = ReleaseRef::new(app(), ReleaseId(44)).artifact(Platform::new("linux", "amd64"));
        assert_eq!(a.store_path(), "applications/demo-stack/releases/44/linux-amd64.bundle.zst");
    }

    #[test]
    fn pinned_target_ignores_the_application_default() {
        let pinned = ReleaseTarget::Pinned { release: ReleaseId(42) };
        assert_eq!(pinned.resolve(Some(ReleaseId(45))), Some(ReleaseId(42)));
    }

    #[test]
    fn default_target_follows_and_tolerates_no_default_yet() {
        assert_eq!(ReleaseTarget::Default.resolve(Some(ReleaseId(45))), Some(ReleaseId(45)));
        assert_eq!(ReleaseTarget::Default.resolve(None), None);
    }
}
