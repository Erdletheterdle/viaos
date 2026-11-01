//! Turning a [`Deployment`] into concrete container specifications.
//!
//! Kept apart from the Docker client so the interesting decisions -- what a
//! container should look like, and whether the one that exists already matches
//! -- are testable without a daemon.

use std::collections::BTreeMap;

use sha2::{Digest as _, Sha256};
use via_core::{Deployment, Service};

use crate::types::{ApplyContext, Naming, resolved_networks};

/// Labels Via stamps on everything it creates.
pub const LABEL_PROJECT: &str = via_core::LABEL_PROJECT;
pub const LABEL_SERVICE: &str = via_core::LABEL_SERVICE;
pub const LABEL_RELEASE: &str = via_core::LABEL_RELEASE;
/// Hash of the desired container configuration, used to decide whether an
/// existing container is current.
pub const LABEL_CONFIG_HASH: &str = "via.config-hash";

/// Fingerprint of everything about a service that requires a new container if
/// it changes.
///
/// This is what makes `apply` idempotent and therefore safe to run on a timer:
/// a service whose hash still matches is left strictly alone, so reconciling
/// after a reboot does not restart a healthy database.
///
/// The release number is deliberately excluded. Including it would recreate
/// every container on every release, discarding uptime for services that did
/// not change.
pub fn config_hash(deployment: &Deployment, service: &Service, ctx: &ApplyContext) -> String {
    let mut hasher = Sha256::new();
    hasher.update(deployment.project.as_bytes());
    hasher.update([0]);

    // Serializing the service is the honest way to cover every field: a field
    // added to `Service` later is included automatically rather than silently
    // dropping out of the fingerprint.
    let service_json = serde_json::to_vec(service).unwrap_or_default();
    hasher.update(&service_json);
    hasher.update([0]);

    // Runtime environment is not part of the release but does require a new
    // container when it changes, since Docker cannot alter a container's
    // environment in place.
    for (k, v) in ctx.effective_env(service) {
        hasher.update(k.as_bytes());
        hasher.update(*b"=");
        hasher.update(v.as_bytes());
        hasher.update([0]);
    }

    // Network attachment is computed rather than stored on the service.
    if let Some(nets) = resolved_networks(deployment, service) {
        for n in nets {
            hasher.update(n.as_bytes());
            hasher.update([0]);
        }
    }

    format!("sha256:{}", hex::encode(hasher.finalize()))
}

/// Labels for a service's container.
pub fn container_labels(
    deployment: &Deployment,
    service: &Service,
    ctx: &ApplyContext,
    hash: &str,
) -> BTreeMap<String, String> {
    let mut labels: BTreeMap<String, String> = service.labels.clone();
    labels.insert(LABEL_PROJECT.into(), deployment.project.clone());
    labels.insert(LABEL_SERVICE.into(), service.name.clone());
    labels.insert(LABEL_CONFIG_HASH.into(), hash.to_string());
    if let Some(release) = ctx.release {
        labels.insert(LABEL_RELEASE.into(), release.to_string());
    }
    labels
}

/// Labels for a network or volume Via manages.
pub fn object_labels(project: &str) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::new();
    labels.insert(LABEL_PROJECT.to_string(), project.to_string());
    labels
}

/// Environment rendered as Docker's `KEY=value` list.
pub fn env_list(service: &Service, ctx: &ApplyContext) -> Vec<String> {
    ctx.effective_env(service)
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect()
}

/// Container name for a service.
pub fn container_name(deployment: &Deployment, service: &Service) -> String {
    Naming::container(&deployment.project, &service.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use via_core::{ImageRef, Platform};

    fn deployment() -> Deployment {
        let mut d = Deployment::new("demo-stack", Platform::new("linux", "arm64"));
        d.services
            .insert("app".into(), Service::new("app", ImageRef::new("app:v1")));
        d
    }

    #[test]
    fn identical_input_hashes_identically() {
        let d = deployment();
        let ctx = ApplyContext::default();
        let a = config_hash(&d, &d.services["app"], &ctx);
        let b = config_hash(&d, &d.services["app"], &ctx);
        assert_eq!(
            a, b,
            "the hash must be stable or every apply would recreate everything"
        );
    }

    #[test]
    fn changing_the_image_changes_the_hash() {
        let d = deployment();
        let ctx = ApplyContext::default();
        let before = config_hash(&d, &d.services["app"], &ctx);

        let mut d2 = d.clone();
        d2.services.get_mut("app").unwrap().image = ImageRef::new("app:v2");
        assert_ne!(before, config_hash(&d2, &d2.services["app"], &ctx));
    }

    #[test]
    fn changing_runtime_environment_changes_the_hash() {
        // A changed secret must produce a new container: Docker cannot alter
        // a running container's environment.
        let d = deployment();
        let plain = config_hash(&d, &d.services["app"], &ApplyContext::default());

        let mut ctx = ApplyContext::default();
        ctx.environment
            .entry("app".into())
            .or_default()
            .insert("TOKEN".into(), "secret".into());
        assert_ne!(plain, config_hash(&d, &d.services["app"], &ctx));
    }

    #[test]
    fn a_declared_runtime_value_reaches_the_service_and_changes_the_hash() {
        // A rotated secret must produce a new container, since Docker cannot
        // change a running container's environment.
        let mut d = deployment();
        d.services.get_mut("app").unwrap().runtime_env = vec!["TOKEN".to_string()];
        let plain = config_hash(&d, &d.services["app"], &ApplyContext::default());

        let mut global = BTreeMap::new();
        global.insert("TOKEN".to_string(), "rotated".to_string());
        let ctx = ApplyContext::default().with_global(global);

        assert_eq!(ctx.effective_env(&d.services["app"])["TOKEN"], "rotated");
        assert_ne!(plain, config_hash(&d, &d.services["app"], &ctx));
    }

    #[test]
    fn a_service_that_never_asked_for_a_value_does_not_receive_it() {
        // This is what stops rotating one credential from restarting the
        // database alongside the service that actually reads it.
        let d = deployment();
        assert!(d.services["app"].runtime_env.is_empty());
        let plain = config_hash(&d, &d.services["app"], &ApplyContext::default());

        let mut global = BTreeMap::new();
        global.insert("SOMEONE_ELSES_TOKEN".to_string(), "value".to_string());
        let ctx = ApplyContext::default().with_global(global);

        assert!(
            !ctx.effective_env(&d.services["app"])
                .contains_key("SOMEONE_ELSES_TOKEN")
        );
        assert_eq!(
            plain,
            config_hash(&d, &d.services["app"], &ctx),
            "an unwanted value must not churn the container"
        );
    }

    #[test]
    fn a_service_override_beats_a_global_value() {
        let mut d = deployment();
        d.services.get_mut("app").unwrap().runtime_env = vec!["TOKEN".to_string()];
        let mut global = BTreeMap::new();
        global.insert("TOKEN".to_string(), "fleet-wide".to_string());
        let mut ctx = ApplyContext::default().with_global(global);
        ctx.environment
            .entry("app".into())
            .or_default()
            .insert("TOKEN".into(), "just-this-service".into());

        assert_eq!(
            ctx.effective_env(&d.services["app"])["TOKEN"],
            "just-this-service"
        );
    }

    #[test]
    fn runtime_values_beat_what_the_release_baked_in() {
        // Otherwise rotating a credential would require rebuilding the bundle.
        // A key present in the baked-in environment counts as declared.
        let mut d = deployment();
        d.services
            .get_mut("app")
            .unwrap()
            .environment
            .insert("TOKEN".into(), "baked-in".into());

        let mut global = BTreeMap::new();
        global.insert("TOKEN".to_string(), "from-the-control-plane".to_string());
        let ctx = ApplyContext::default().with_global(global);

        assert_eq!(
            ctx.effective_env(&d.services["app"])["TOKEN"],
            "from-the-control-plane"
        );
    }

    #[test]
    fn the_release_number_alone_does_not_change_the_hash() {
        // Otherwise every release would restart every service, including the
        // database, for no reason.
        let d = deployment();
        let a = config_hash(
            &d,
            &d.services["app"],
            &ApplyContext::for_release(via_core::ReleaseId(44)),
        );
        let b = config_hash(
            &d,
            &d.services["app"],
            &ApplyContext::for_release(via_core::ReleaseId(45)),
        );
        assert_eq!(a, b);
    }

    #[test]
    fn a_different_project_hashes_differently() {
        let d = deployment();
        let mut other = d.clone();
        other.project = "other-app".into();
        let ctx = ApplyContext::default();
        assert_ne!(
            config_hash(&d, &d.services["app"], &ctx),
            config_hash(&other, &other.services["app"], &ctx)
        );
    }
}
