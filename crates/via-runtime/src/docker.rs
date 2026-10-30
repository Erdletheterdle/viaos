//! Docker implementation of [`Runtime`].
//!
//! Talks to the Docker Engine API directly rather than shelling out to the
//! `docker` binary (spec section 17): shelling out means parsing human output,
//! and it makes the agent depend on a CLI that may not exist on a
//! container-only host.

use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::models::{
    ContainerCreateBody, ContainerStateStatusEnum, DeviceMapping as DockerDevice, EndpointSettings,
    HealthConfig, HealthStatusEnum, HostConfig, Mount as DockerMount, MountTmpfsOptions, MountType,
    NetworkCreateRequest, NetworkingConfig, PortBinding, RestartPolicy as DockerRestartPolicy,
    RestartPolicyNameEnum, VolumeCreateRequest,
};
use bollard::query_parameters as qp;
use futures::StreamExt;
use tokio::time::{Duration, Instant, sleep};
use via_core::{Deployment, Mount, Readiness, RestartPolicy, Service};

use crate::plan::*;
use crate::types::*;
use crate::{Error, Runtime};

/// Nanoseconds per second, the unit Docker's healthcheck fields use.
const NANOS: i64 = 1_000_000_000;

/// How often to re-poll while waiting on a dependency or a health gate.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub struct DockerRuntime {
    docker: Docker,
}

impl DockerRuntime {
    /// Connect using the platform default, honoring `DOCKER_HOST`.
    pub fn connect() -> Result<Self, Error> {
        let docker = Docker::connect_with_defaults().map_err(Error::docker)?;
        Ok(Self { docker })
    }

    pub fn from_client(docker: Docker) -> Self {
        Self { docker }
    }

    pub fn client(&self) -> &Docker {
        &self.docker
    }

    /// Containers Via owns for a project, whether or not they are running.
    async fn project_containers(
        &self,
        project: &str,
    ) -> Result<Vec<bollard::models::ContainerSummary>, Error> {
        let mut filters = HashMap::new();
        filters.insert("label".to_string(), vec![format!("{LABEL_PROJECT}={project}")]);
        let options = qp::ListContainersOptionsBuilder::new().all(true).filters(&filters).build();
        self.docker.list_containers(Some(options)).await.map_err(Error::docker)
    }

    async fn inspect_service(&self, name: &str) -> Result<Option<ServiceState>, Error> {
        let inspect = match self.docker.inspect_container(name, None::<qp::InspectContainerOptions>).await {
            Ok(c) => c,
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {
                return Ok(None);
            }
            Err(e) => return Err(Error::docker(e)),
        };

        let labels = inspect.config.as_ref().and_then(|c| c.labels.clone()).unwrap_or_default();
        let state = inspect.state.as_ref();

        let status = match state.and_then(|s| s.status) {
            Some(ContainerStateStatusEnum::RUNNING) => ContainerStatus::Running,
            Some(ContainerStateStatusEnum::CREATED) => ContainerStatus::Created,
            Some(ContainerStateStatusEnum::RESTARTING) => ContainerStatus::Restarting,
            Some(ContainerStateStatusEnum::PAUSED) => ContainerStatus::Paused,
            Some(ContainerStateStatusEnum::DEAD) => ContainerStatus::Dead,
            Some(ContainerStateStatusEnum::EXITED) => {
                ContainerStatus::Exited { code: state.and_then(|s| s.exit_code).unwrap_or(0) }
            }
            _ => ContainerStatus::Created,
        };

        let health = match state.and_then(|s| s.health.as_ref()).and_then(|h| h.status) {
            Some(HealthStatusEnum::HEALTHY) => HealthStatus::Healthy,
            Some(HealthStatusEnum::UNHEALTHY) => HealthStatus::Unhealthy,
            Some(HealthStatusEnum::STARTING) => HealthStatus::Starting,
            _ => HealthStatus::None,
        };

        Ok(Some(ServiceState {
            service: labels.get(LABEL_SERVICE).cloned().unwrap_or_default(),
            container_id: inspect.id,
            container_name: inspect.name.map(|n| n.trim_start_matches('/').to_string()),
            status,
            health,
            config_hash: labels.get(LABEL_CONFIG_HASH).cloned(),
            release: labels.get(LABEL_RELEASE).and_then(|r| r.parse().ok()),
            image: inspect.config.and_then(|c| c.image),
        }))
    }

    /// Create the networks a deployment needs, skipping external ones.
    async fn ensure_networks(&self, deployment: &Deployment, report: &mut ApplyReport) -> Result<(), Error> {
        let existing: Vec<String> = self
            .docker
            .list_networks(None::<qp::ListNetworksOptions>)
            .await
            .map_err(Error::docker)?
            .into_iter()
            .filter_map(|n| n.name)
            .collect();

        let mut wanted: Vec<(String, Option<&via_core::Network>)> = Vec::new();
        if needs_default_network(deployment) {
            wanted.push((Naming::default_network(&deployment.project), None));
        }
        for (name, net) in &deployment.networks {
            if net.external {
                // Via did not create it and must never create or delete it.
                if !existing.iter().any(|e| e == name) {
                    return Err(Error::MissingExternalNetwork { name: name.clone() });
                }
                continue;
            }
            wanted.push((Naming::network(&deployment.project, name), Some(net)));
        }

        for (full_name, spec) in wanted {
            if existing.iter().any(|e| e == &full_name) {
                continue;
            }
            let request = NetworkCreateRequest {
                name: full_name.clone(),
                driver: spec.and_then(|s| s.driver.clone()).or_else(|| Some("bridge".into())),
                internal: spec.map(|s| s.internal),
                options: spec.map(|s| s.driver_opts.clone().into_iter().collect()),
                labels: Some(merge_labels(object_labels(&deployment.project), spec.map(|s| &s.labels))),
                ..Default::default()
            };
            self.docker.create_network(request).await.map_err(Error::docker)?;
            report.networks_created.push(full_name);
        }
        Ok(())
    }

    /// Create named volumes that do not yet exist.
    ///
    /// Existing volumes are never touched: their whole purpose is to outlive
    /// releases (spec section 19).
    async fn ensure_volumes(&self, deployment: &Deployment, report: &mut ApplyReport) -> Result<(), Error> {
        let existing: Vec<String> = self
            .docker
            .list_volumes(None::<qp::ListVolumesOptions>)
            .await
            .map_err(Error::docker)?
            .volumes
            .unwrap_or_default()
            .into_iter()
            .map(|v| v.name)
            .collect();

        // Anonymous volumes are synthesized by the normalizer and appear only
        // on a mount, never in `volumes:`.
        let mut wanted: Vec<(String, Option<&via_core::Volume>)> = Vec::new();
        for (name, vol) in &deployment.volumes {
            if vol.external {
                if !existing.iter().any(|e| e == name) {
                    return Err(Error::MissingExternalVolume { name: name.clone() });
                }
                continue;
            }
            wanted.push((Naming::volume(&deployment.project, name), Some(vol)));
        }
        for svc in deployment.services.values() {
            for m in &svc.mounts {
                if let Mount::Volume { source, .. } = m {
                    if !deployment.volumes.contains_key(source) {
                        let full = Naming::volume(&deployment.project, source);
                        if !wanted.iter().any(|(n, _)| n == &full) {
                            wanted.push((full, None));
                        }
                    }
                }
            }
        }

        for (full_name, spec) in wanted {
            if existing.iter().any(|e| e == &full_name) {
                continue;
            }
            let options = VolumeCreateRequest {
                name: Some(full_name.clone()),
                driver: spec.and_then(|s| s.driver.clone()),
                driver_opts: spec.map(|s| s.driver_opts.clone().into_iter().collect()),
                labels: Some(merge_labels(object_labels(&deployment.project), spec.map(|s| &s.labels))),
                ..Default::default()
            };
            self.docker.create_volume(options).await.map_err(Error::docker)?;
            report.volumes_created.push(full_name);
        }
        Ok(())
    }

    /// Wait until a service satisfies a `depends_on` condition.
    async fn wait_for_condition(
        &self,
        deployment: &Deployment,
        dep: &via_core::Dependency,
        deadline: Instant,
    ) -> Result<(), Error> {
        use via_core::DependencyCondition::*;
        let Some(service) = deployment.services.get(&dep.service) else { return Ok(()) };
        let name = container_name(deployment, service);

        loop {
            let state = self.inspect_service(&name).await?;
            let satisfied = match (&dep.condition, &state) {
                (Started, Some(s)) => s.status.is_running() || matches!(s.status, ContainerStatus::Exited { .. }),
                (Healthy, Some(s)) => s.health == HealthStatus::Healthy,
                (CompletedSuccessfully, Some(s)) => matches!(s.status, ContainerStatus::Exited { code: 0 }),
                (_, None) => false,
            };
            if satisfied {
                return Ok(());
            }
            // A dependency that has already failed will never be satisfied, so
            // fail now rather than burning the whole gate timeout.
            if let Some(s) = &state {
                if let ContainerStatus::Exited { code } = s.status {
                    if code != 0 && !matches!(dep.condition, Started) {
                        return Err(Error::DependencyFailed {
                            service: dep.service.clone(),
                            reason: format!("exited with code {code}"),
                        });
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(Error::DependencyTimeout {
                    service: dep.service.clone(),
                    condition: format!("{:?}", dep.condition),
                });
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn remove_container(&self, name: &str) -> Result<(), Error> {
        let options = qp::RemoveContainerOptionsBuilder::new().force(true).v(false).build();
        match self.docker.remove_container(name, Some(options)).await {
            Ok(()) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => Ok(()),
            Err(e) => Err(Error::docker(e)),
        }
    }

    /// Build the container specification for a service.
    fn container_config(
        &self,
        deployment: &Deployment,
        service: &Service,
        ctx: &ApplyContext,
        hash: &str,
    ) -> ContainerCreateBody {
        let mut mounts = Vec::new();
        for m in &service.mounts {
            mounts.push(match m {
                Mount::Volume { source, target, read_only } => DockerMount {
                    typ: Some(MountType::VOLUME),
                    // Volumes declared in the manifest are namespaced by
                    // project; external ones keep their real name.
                    source: Some(match deployment.volumes.get(source) {
                        Some(v) if v.external => source.clone(),
                        _ => Naming::volume(&deployment.project, source),
                    }),
                    target: Some(target.clone()),
                    read_only: Some(*read_only),
                    ..Default::default()
                },
                Mount::Bind { source, target, read_only } => DockerMount {
                    typ: Some(MountType::BIND),
                    source: Some(source.clone()),
                    target: Some(target.clone()),
                    read_only: Some(*read_only),
                    ..Default::default()
                },
                Mount::Tmpfs { target, size_bytes } => DockerMount {
                    typ: Some(MountType::TMPFS),
                    target: Some(target.clone()),
                    tmpfs_options: Some(MountTmpfsOptions {
                        size_bytes: size_bytes.map(|s| s as i64),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            });
        }

        let mut port_bindings: HashMap<String, Option<Vec<PortBinding>>> = HashMap::new();
        let mut exposed = Vec::new();
        for p in &service.ports {
            let key = format!("{}/{}", p.container_port, p.protocol);
            exposed.push(key.clone());
            port_bindings.entry(key).or_insert_with(|| Some(Vec::new())).as_mut().unwrap().push(
                PortBinding {
                    host_ip: p.host_ip.clone(),
                    // An empty host port asks Docker for an ephemeral one,
                    // which is what Compose's bare `- "80"` means.
                    host_port: Some(p.host_port.map(|h| h.to_string()).unwrap_or_default()),
                },
            );
        }

        let restart_policy = Some(match &service.restart {
            RestartPolicy::No => DockerRestartPolicy {
                name: Some(RestartPolicyNameEnum::NO),
                maximum_retry_count: None,
            },
            RestartPolicy::Always => DockerRestartPolicy {
                name: Some(RestartPolicyNameEnum::ALWAYS),
                maximum_retry_count: None,
            },
            RestartPolicy::UnlessStopped => DockerRestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            },
            RestartPolicy::OnFailure { max_retries } => DockerRestartPolicy {
                name: Some(RestartPolicyNameEnum::ON_FAILURE),
                maximum_retry_count: max_retries.map(|r| r as i64),
            },
        });

        let healthcheck = service.healthcheck.as_ref().map(|hc| HealthConfig {
            test: Some(hc.test.clone()),
            interval: hc.interval_secs.map(|s| s as i64 * NANOS),
            timeout: hc.timeout_secs.map(|s| s as i64 * NANOS),
            retries: hc.retries.map(|r| r as i64),
            start_period: hc.start_period_secs.map(|s| s as i64 * NANOS),
            start_interval: None,
        });

        let networking_config = resolved_networks(deployment, service).map(|nets| {
            let mut endpoints = HashMap::new();
            for n in nets {
                endpoints.insert(
                    n,
                    EndpointSettings {
                        // The service name must resolve on the network, or
                        // `depends_on` peers cannot address each other the way
                        // they do under Compose.
                        aliases: Some(vec![service.name.clone()]),
                        ..Default::default()
                    },
                );
            }
            NetworkingConfig { endpoints_config: Some(endpoints) }
        });

        let host_config = HostConfig {
            mounts: (!mounts.is_empty()).then_some(mounts),
            port_bindings: (!port_bindings.is_empty()).then_some(port_bindings),
            restart_policy,
            network_mode: service.network_mode.clone(),
            privileged: Some(service.privileged),
            cap_add: (!service.cap_add.is_empty()).then(|| service.cap_add.clone()),
            cap_drop: (!service.cap_drop.is_empty()).then(|| service.cap_drop.clone()),
            devices: (!service.devices.is_empty()).then(|| {
                service
                    .devices
                    .iter()
                    .map(|d| DockerDevice {
                        path_on_host: Some(d.host_path.clone()),
                        path_in_container: Some(d.effective_container_path().to_string()),
                        cgroup_permissions: Some(d.permissions.clone()),
                    })
                    .collect()
            }),
            extra_hosts: (!service.extra_hosts.is_empty()).then(|| service.extra_hosts.clone()),
            dns: (!service.dns.is_empty()).then(|| service.dns.clone()),
            group_add: (!service.group_add.is_empty()).then(|| service.group_add.clone()),
            sysctls: (!service.sysctls.is_empty())
                .then(|| service.sysctls.clone().into_iter().collect()),
            init: service.init,
            pid_mode: service.pid.clone(),
            ipc_mode: service.ipc.clone(),
            shm_size: service.shm_size_bytes.map(|s| s as i64),
            readonly_rootfs: Some(service.read_only),
            ..Default::default()
        };

        ContainerCreateBody {
            image: Some(service.image.reference.clone()),
            cmd: service.command.clone(),
            entrypoint: service.entrypoint.clone(),
            env: Some(env_list(service, ctx)),
            labels: Some(container_labels(deployment, service, ctx, hash).into_iter().collect()),
            hostname: service.hostname.clone().or_else(|| Some(service.name.clone())),
            user: service.user.clone(),
            working_dir: service.working_dir.clone(),
            tty: Some(service.tty),
            open_stdin: Some(service.stdin_open),
            stop_signal: service.stop_signal.clone(),
            stop_timeout: service.stop_grace_period_secs.map(|s| s as i64),
            exposed_ports: (!exposed.is_empty()).then_some(exposed),
            healthcheck,
            host_config: Some(host_config),
            networking_config,
            ..Default::default()
        }
    }
}

#[async_trait]
impl Runtime for DockerRuntime {
    async fn info(&self) -> Result<RuntimeInfo, Error> {
        let version = self.docker.version().await.map_err(Error::docker)?;
        let info = self.docker.info().await.map_err(Error::docker)?;
        Ok(RuntimeInfo {
            engine: "docker".into(),
            version: version.version.unwrap_or_default(),
            api_version: version.api_version,
            os: version.os,
            arch: version.arch,
            kernel: version.kernel_version,
            root_dir: info.docker_root_dir,
        })
    }

    async fn load_images(&self, archive: &Path) -> Result<Vec<String>, Error> {
        let file = tokio::fs::File::open(archive).await.map_err(|e| Error::io(archive, e))?;
        let stream = tokio_util::io::ReaderStream::new(file);
        let options = qp::ImportImageOptionsBuilder::new().quiet(true).build();

        let mut loaded = Vec::new();
        let mut results = self.docker.import_image_stream(options, stream, None);
        while let Some(item) = results.next().await {
            let info = item.map_err(Error::docker)?;
            if let Some(stream) = info.stream {
                // Docker reports each image as "Loaded image: <ref>".
                if let Some(reference) = stream.trim().strip_prefix("Loaded image: ") {
                    loaded.push(reference.to_string());
                }
            }
        }
        Ok(loaded)
    }

    async fn image_present(&self, reference: &str) -> Result<bool, Error> {
        match self.docker.inspect_image(reference).await {
            Ok(_) => Ok(true),
            Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => Ok(false),
            Err(e) => Err(Error::docker(e)),
        }
    }

    async fn apply(&self, deployment: &Deployment, ctx: &ApplyContext) -> Result<ApplyReport, Error> {
        let mut report = ApplyReport::default();

        // Refuse to start anything if an image is missing. A bundle is
        // self-contained by definition, so a missing image means the release
        // is broken, not that something should be fetched.
        for image in deployment.required_images() {
            if !self.image_present(&image.reference).await? {
                return Err(Error::ImageNotLoaded { reference: image.reference.clone() });
            }
        }

        self.ensure_networks(deployment, &mut report).await?;
        self.ensure_volumes(deployment, &mut report).await?;

        let order = deployment.start_order().map_err(|c| Error::DependencyCycle { path: c.to_string() })?;
        let deadline = Instant::now() + Duration::from_secs(deployment.health_gate.timeout_secs);

        for service_name in &order {
            let service = &deployment.services[*service_name];
            let name = container_name(deployment, service);
            let hash = config_hash(deployment, service, ctx);

            for dep in &service.depends_on {
                self.wait_for_condition(deployment, dep, deadline).await?;
            }

            let existing = self.inspect_service(&name).await?;
            let up_to_date = existing
                .as_ref()
                .is_some_and(|s| s.config_hash.as_deref() == Some(hash.as_str()));

            if up_to_date {
                let state = existing.as_ref().unwrap();
                // Configuration matches; only start it if it is not already up.
                // Leaving a healthy container alone is what makes reconciling
                // on a timer safe.
                match state.status {
                    ContainerStatus::Running | ContainerStatus::Restarting => {
                        report.unchanged.push(service.name.clone());
                    }
                    ContainerStatus::Exited { code: 0 } if service.readiness == Readiness::Completed => {
                        report.unchanged.push(service.name.clone());
                    }
                    _ => {
                        self.docker
                            .start_container(&name, None::<qp::StartContainerOptions>)
                            .await
                            .map_err(Error::docker)?;
                        report.unchanged.push(service.name.clone());
                        report.started.push(service.name.clone());
                    }
                }
                continue;
            }

            let replacing = existing.is_some();
            if replacing {
                self.remove_container(&name).await?;
            }

            let config = self.container_config(deployment, service, ctx, &hash);
            let options = qp::CreateContainerOptionsBuilder::new().name(&name).build();
            self.docker.create_container(Some(options), config).await.map_err(Error::docker)?;
            self.docker
                .start_container(&name, None::<qp::StartContainerOptions>)
                .await
                .map_err(Error::docker)?;

            if replacing {
                report.replaced.push(service.name.clone());
            } else {
                report.created.push(service.name.clone());
            }
            report.started.push(service.name.clone());
        }

        // Anything labeled for this project but no longer in the deployment is
        // an orphan from a previous release and must go, or a removed service
        // would keep running forever.
        for container in self.project_containers(&deployment.project).await? {
            let labels = container.labels.unwrap_or_default();
            let Some(service) = labels.get(LABEL_SERVICE) else { continue };
            if deployment.services.contains_key(service) {
                continue;
            }
            if let Some(names) = container.names {
                if let Some(name) = names.first() {
                    let name = name.trim_start_matches('/');
                    self.remove_container(name).await?;
                    report.removed_orphans.push(service.clone());
                }
            }
        }

        Ok(report)
    }

    async fn service_states(&self, deployment: &Deployment) -> Result<Vec<ServiceState>, Error> {
        let mut states = Vec::new();
        for (name, service) in &deployment.services {
            let container = container_name(deployment, service);
            match self.inspect_service(&container).await? {
                Some(mut s) => {
                    if s.service.is_empty() {
                        s.service = name.clone();
                    }
                    states.push(s);
                }
                None => states.push(ServiceState::absent(name)),
            }
        }
        Ok(states)
    }

    async fn logs(&self, deployment: &Deployment, options: &LogOptions) -> Result<Logs, Error> {
        // Named services are checked against the manifest first, so asking for
        // a service that does not exist is an error rather than an empty
        // stream that looks like a silent application.
        for wanted in &options.services {
            if !deployment.services.contains_key(wanted) {
                return Err(Error::NoSuchService { service: wanted.clone() });
            }
        }

        // Docker reads `tail` as a string so that "all" can mean all.
        let tail = options.tail.to_string();

        let mut services = Vec::new();
        let mut streams = Vec::new();
        for (name, service) in &deployment.services {
            if !options.services.is_empty() && !options.services.contains(name) {
                continue;
            }
            let container = container_name(deployment, service);
            // A service with no container is skipped rather than failing the
            // whole request: one service that has not started must not hide
            // the output of the ones that did -- which is usually where the
            // reason it did not start is written.
            if self.inspect_service(&container).await?.is_none() {
                continue;
            }

            let params = qp::LogsOptionsBuilder::new()
                .follow(options.follow)
                .stdout(true)
                .stderr(true)
                // The device's clock, taken from the engine. An operator
                // correlating a log line with a device event needs the
                // device's idea of when it happened, not the control plane's.
                .timestamps(true)
                .tail(&tail)
                .build();

            let chunks = self.docker.logs(&container, Some(params)).map(|item| {
                item.map(|output| match output {
                    LogOutput::StdErr { message } => (LogSource::Stderr, message.to_vec()),
                    LogOutput::StdOut { message }
                    | LogOutput::StdIn { message }
                    | LogOutput::Console { message } => (LogSource::Stdout, message.to_vec()),
                })
                .map_err(Error::docker)
            });

            services.push(name.clone());
            streams.push(crate::logs::lines(name.clone(), chunks).boxed());
        }

        Ok(Logs { services, lines: futures::stream::select_all(streams).boxed() })
    }

    async fn exec(&self, deployment: &Deployment, options: &ExecOptions) -> Result<Exec, Error> {
        if options.command.is_empty() {
            return Err(Error::Invalid { reason: "no command was given to run".into() });
        }
        let service = deployment
            .services
            .get(&options.service)
            .ok_or_else(|| Error::NoSuchService { service: options.service.clone() })?;
        let container = container_name(deployment, service);
        // A service that is not running is an error rather than an empty
        // stream: "the command produced no output" and "there was nothing to
        // run it in" are opposite answers to a debugging question.
        if self.inspect_service(&container).await?.is_none() {
            return Err(Error::NoSuchService { service: options.service.clone() });
        }

        let created = self
            .docker
            .create_exec(
                &container,
                bollard::models::ExecConfig {
                    cmd: Some(options.command.clone()),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    // No stdin and no TTY. Both are what an interactive shell
                    // needs, and this is deliberately not one.
                    attach_stdin: Some(false),
                    tty: Some(false),
                    ..Default::default()
                },
            )
            .await
            .map_err(Error::docker)?;

        let started = self.docker.start_exec(&created.id, None).await.map_err(Error::docker)?;
        let bollard::exec::StartExecResults::Attached { output, .. } = started else {
            return Err(Error::Invalid {
                reason: "the engine detached the command instead of attaching to it".into(),
            });
        };

        // The exit status arrives only after the output ends, from a separate
        // inspect -- Docker does not put it in the stream. Chained on so a
        // caller sees one sequence that finishes with the answer.
        let docker = self.docker.clone();
        let exec_id = created.id.clone();
        let body = output
            .map(|item| {
                item.map(|out| match out {
                    LogOutput::StdErr { message } => ExecChunk::Output {
                        source: LogSource::Stderr,
                        data: String::from_utf8_lossy(&message).into_owned(),
                    },
                    LogOutput::StdOut { message }
                    | LogOutput::StdIn { message }
                    | LogOutput::Console { message } => ExecChunk::Output {
                        source: LogSource::Stdout,
                        data: String::from_utf8_lossy(&message).into_owned(),
                    },
                })
                .map_err(Error::docker)
            })
            .chain(futures::stream::once(async move {
                let status = docker.inspect_exec(&exec_id).await.map_err(Error::docker)?;
                Ok(ExecChunk::Exit { code: status.exit_code })
            }));

        Ok(Exec { container, output: body.boxed() })
    }

    async fn stop(&self, deployment: &Deployment) -> Result<(), Error> {
        // Reverse dependency order, so dependents stop before what they need.
        let order = deployment.start_order().map_err(|c| Error::DependencyCycle { path: c.to_string() })?;
        for service_name in order.iter().rev() {
            let service = &deployment.services[*service_name];
            let name = container_name(deployment, service);
            let options = qp::StopContainerOptionsBuilder::new()
                .t(service.stop_grace_period_secs.unwrap_or(10) as i32)
                .build();
            match self.docker.stop_container(&name, Some(options)).await {
                Ok(()) => {}
                Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => {}
                Err(bollard::errors::Error::DockerResponseServerError { status_code: 304, .. }) => {}
                Err(e) => return Err(Error::docker(e)),
            }
        }
        Ok(())
    }

    async fn remove(&self, deployment: &Deployment, remove_volumes: bool) -> Result<(), Error> {
        self.stop(deployment).await?;
        for container in self.project_containers(&deployment.project).await? {
            if let Some(names) = container.names {
                if let Some(name) = names.first() {
                    self.remove_container(name.trim_start_matches('/')).await?;
                }
            }
        }
        if remove_volumes {
            for name in deployment.volumes.keys() {
                let full = Naming::volume(&deployment.project, name);
                let _ = self
                    .docker
                    .remove_volume(&full, None::<qp::RemoveVolumeOptions>)
                    .await;
            }
        }
        Ok(())
    }
}

fn merge_labels(
    mut base: std::collections::BTreeMap<String, String>,
    extra: Option<&std::collections::BTreeMap<String, String>>,
) -> HashMap<String, String> {
    if let Some(extra) = extra {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    base.into_iter().collect()
}
