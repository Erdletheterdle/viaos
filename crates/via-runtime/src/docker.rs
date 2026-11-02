//! Docker implementation of [`Runtime`].
//!
//! `docker compose` decides what runs and starts it (see [`crate::compose`]).
//! The Engine API is for reading what it started: it answers in structured
//! data, and logs and stats come as streams.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use async_trait::async_trait;
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::models::{ContainerStateStatusEnum, ContainerSummary, HealthStatusEnum};
use bollard::query_parameters as qp;
use futures::StreamExt;
use via_core::{Deployment, LABEL_LEGACY_PROJECT, LABEL_ONE_OFF, LABEL_PROJECT, LABEL_SERVICE};

use crate::types::*;
use crate::{Error, Runtime};

/// What `docker stats` counts as in use: page cache the kernel can drop at
/// once isn't memory the service is holding on to.
fn memory_in_use(usage: u64, inactive_file: Option<u64>) -> u64 {
    usage.saturating_sub(inactive_file.unwrap_or(0))
}

/// One reading of one service. The CPU counters only mean something compared
/// with an earlier reading.
#[derive(Debug, Clone)]
pub struct ServiceStats {
    pub service: String,
    pub memory_bytes: u64,
    pub cpu_total_ns: u64,
    pub system_cpu_ns: u64,
    pub online_cpus: u32,
}

#[derive(Debug, Clone)]
pub struct VolumeUsage {
    /// The name Docker knows it by; see [`Deployment::volume_name`].
    pub name: String,
    pub bytes: u64,
    /// How many containers use it.
    pub containers: u64,
}

#[derive(Debug, Clone, Default)]
pub struct DiskUsage {
    pub images_bytes: u64,
    pub containers_bytes: u64,
    pub volumes_bytes: u64,
    pub build_cache_bytes: u64,
    pub volumes: Vec<VolumeUsage>,
}

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

    /// What each running service of a deployment is using now. Cheap enough to
    /// call every few seconds: `one_shot` skips Docker's usual one-second wait
    /// for a second CPU reading, and the caller compares samples itself.
    pub async fn service_stats(&self, deployment: &Deployment) -> Result<Vec<ServiceStats>, Error> {
        let mut out = Vec::new();
        let containers = self.containers(&deployment.project).await?;
        for service in deployment.services.keys() {
            let mut total: Option<ServiceStats> = None;
            for name in containers_of(&containers, service) {
                let options = qp::StatsOptionsBuilder::new()
                    .stream(false)
                    .one_shot(true)
                    .build();
                let mut stream = self.docker.stats(&name, Some(options));
                // A container that just went away isn't in the answer.
                let Some(Ok(stats)) = stream.next().await else {
                    continue;
                };
                let memory = stats.memory_stats.unwrap_or_default();
                let Some(usage) = memory.usage else {
                    continue;
                };
                let inactive = memory.stats.as_ref().and_then(|s| {
                    s.get("inactive_file")
                        .or_else(|| s.get("total_inactive_file"))
                        .copied()
                });
                let cpu = stats.cpu_stats.unwrap_or_default();
                let total = total.get_or_insert(ServiceStats {
                    service: service.clone(),
                    memory_bytes: 0,
                    cpu_total_ns: 0,
                    system_cpu_ns: cpu.system_cpu_usage.unwrap_or(0),
                    online_cpus: cpu.online_cpus.unwrap_or(1),
                });
                total.memory_bytes += memory_in_use(usage, inactive);
                total.cpu_total_ns += cpu.cpu_usage.and_then(|c| c.total_usage).unwrap_or(0);
            }
            out.extend(total);
        }
        Ok(out)
    }

    /// How much disk Docker is holding. This can walk every volume, so it isn't
    /// for calling often.
    pub async fn disk_usage(&self) -> Result<DiskUsage, Error> {
        let options = qp::DataUsageOptionsBuilder::new().verbose(true).build();
        let df = self.docker.df(Some(options)).await.map_err(Error::docker)?;
        let bytes = |v: Option<i64>| v.unwrap_or(0).max(0) as u64;
        let (volume_total, items) = df
            .volume_usage
            .map(|v| (v.total_size, v.items.unwrap_or_default()))
            .unwrap_or((None, Vec::new()));
        let volumes = items
            .iter()
            .filter_map(|v| {
                Some(VolumeUsage {
                    name: v["Name"].as_str()?.to_string(),
                    bytes: v["UsageData"]["Size"].as_i64().unwrap_or(0).max(0) as u64,
                    containers: v["UsageData"]["RefCount"].as_i64().unwrap_or(0).max(0) as u64,
                })
            })
            .collect();
        Ok(DiskUsage {
            images_bytes: bytes(df.image_usage.and_then(|u| u.total_size)),
            containers_bytes: bytes(df.container_usage.and_then(|u| u.total_size)),
            volumes_bytes: bytes(volume_total),
            build_cache_bytes: bytes(df.build_cache_usage.and_then(|u| u.total_size)),
            volumes,
        })
    }

    /// A project's containers, stopped ones too, without `docker compose run`
    /// one-offs.
    async fn containers(&self, project: &str) -> Result<Vec<ContainerSummary>, Error> {
        let mut containers = self
            .labelled_containers(&format!("{LABEL_PROJECT}={project}"))
            .await?;
        containers.retain(|c| {
            c.labels
                .as_ref()
                .and_then(|l| l.get(LABEL_ONE_OFF))
                .is_none_or(|v| v != "True")
        });
        Ok(containers)
    }

    async fn labelled_containers(&self, label: &str) -> Result<Vec<ContainerSummary>, Error> {
        let filters = HashMap::from([("label".to_string(), vec![label.to_string()])]);
        let options = qp::ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters)
            .build();
        self.docker
            .list_containers(Some(options))
            .await
            .map_err(Error::docker)
    }

    /// Remove what viaos started before it ran Compose, which would otherwise
    /// hold the same ports. Its volumes stay: they have the names Compose uses.
    async fn retire_legacy(&self, project: &str) -> Result<(), Error> {
        let label = format!("{LABEL_LEGACY_PROJECT}={project}");
        for container in self.labelled_containers(&label).await? {
            let Some(id) = container.id else { continue };
            let options = qp::RemoveContainerOptionsBuilder::new().force(true).build();
            match self.docker.remove_container(&id, Some(options)).await {
                Ok(()) => {}
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => {}
                Err(e) => return Err(Error::docker(e)),
            }
        }
        let filters = HashMap::from([("label".to_string(), vec![label])]);
        let options = qp::ListNetworksOptionsBuilder::new()
            .filters(&filters)
            .build();
        let networks = self
            .docker
            .list_networks(Some(options))
            .await
            .map_err(Error::docker)?;
        for network in networks {
            if let Some(id) = network.id {
                self.docker
                    .remove_network(&id)
                    .await
                    .map_err(Error::docker)?;
            }
        }
        Ok(())
    }

    /// Each service's container ids.
    async fn ids(&self, project: &str) -> Result<BTreeMap<String, BTreeSet<String>>, Error> {
        let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for c in self.containers(project).await? {
            if let (Some(service), Some(id)) =
                (c.labels.as_ref().and_then(|l| l.get(LABEL_SERVICE)), c.id)
            {
                out.entry(service.clone()).or_default().insert(id);
            }
        }
        Ok(out)
    }

    async fn inspect_service(&self, name: &str) -> Result<Option<ServiceState>, Error> {
        let inspect = match self
            .docker
            .inspect_container(name, None::<qp::InspectContainerOptions>)
            .await
        {
            Ok(c) => c,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => {
                return Ok(None);
            }
            Err(e) => return Err(Error::docker(e)),
        };

        let labels = inspect
            .config
            .as_ref()
            .and_then(|c| c.labels.clone())
            .unwrap_or_default();
        let state = inspect.state.as_ref();

        let status = match state.and_then(|s| s.status) {
            Some(ContainerStateStatusEnum::RUNNING) => ContainerStatus::Running,
            Some(ContainerStateStatusEnum::CREATED) => ContainerStatus::Created,
            Some(ContainerStateStatusEnum::RESTARTING) => ContainerStatus::Restarting,
            Some(ContainerStateStatusEnum::PAUSED) => ContainerStatus::Paused,
            Some(ContainerStateStatusEnum::DEAD) => ContainerStatus::Dead,
            Some(ContainerStateStatusEnum::EXITED) => ContainerStatus::Exited {
                code: state.and_then(|s| s.exit_code).unwrap_or(0),
            },
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
            image: inspect.config.and_then(|c| c.image),
        }))
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
        let file = tokio::fs::File::open(archive)
            .await
            .map_err(|e| Error::io(archive, e))?;
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
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(false),
            Err(e) => Err(Error::docker(e)),
        }
    }

    async fn apply(
        &self,
        deployment: &Deployment,
        ctx: &ApplyContext,
    ) -> Result<ApplyReport, Error> {
        let Some(dir) = &ctx.project_dir else {
            return Err(Error::NoComposeFile);
        };
        // A missing image means the release is broken, and saying which beats
        // whatever a failed `up` would. A release brings its images.
        for image in deployment.required_images() {
            if !self.image_present(&image.reference).await? {
                return Err(Error::ImageNotLoaded {
                    reference: image.reference.clone(),
                });
            }
        }

        self.retire_legacy(&deployment.project).await?;
        let before = self.ids(&deployment.project).await?;
        crate::compose::up(deployment, ctx, dir).await?;
        let after = self.ids(&deployment.project).await?;

        let mut report = ApplyReport::default();
        for (service, ids) in &after {
            match before.get(service) {
                None => report.created.push(service.clone()),
                Some(old) if old == ids => report.unchanged.push(service.clone()),
                Some(_) => report.replaced.push(service.clone()),
            }
        }
        report.removed_orphans = before
            .into_keys()
            .filter(|s| !after.contains_key(s))
            .collect();
        Ok(report)
    }

    async fn service_states(&self, deployment: &Deployment) -> Result<Vec<ServiceState>, Error> {
        let containers = self.containers(&deployment.project).await?;
        let mut states = Vec::new();
        for name in deployment.services.keys() {
            // With replicas, a service is as well as its worst container.
            let mut worst: Option<ServiceState> = None;
            for container in containers_of(&containers, name) {
                let Some(mut state) = self.inspect_service(&container).await? else {
                    continue;
                };
                state.service = name.clone();
                if worst.as_ref().is_none_or(|w| trouble(&state) > trouble(w)) {
                    worst = Some(state);
                }
            }
            states.push(worst.unwrap_or_else(|| ServiceState::absent(name)));
        }
        Ok(states)
    }

    async fn logs(&self, deployment: &Deployment, options: &LogOptions) -> Result<Logs, Error> {
        // Named services are checked against the manifest first, so asking for
        // a service that does not exist is an error rather than an empty
        // stream that looks like a silent application.
        for wanted in &options.services {
            if !deployment.services.contains_key(wanted) {
                return Err(Error::NoSuchService {
                    service: wanted.clone(),
                });
            }
        }

        // Docker reads `tail` as a string so that "all" can mean all.
        let tail = options.tail.to_string();

        let containers = self.containers(&deployment.project).await?;
        let mut services = Vec::new();
        let mut streams = Vec::new();
        for name in deployment.services.keys() {
            if !options.services.is_empty() && !options.services.contains(name) {
                continue;
            }
            // A service with no container is skipped rather than failing the
            // whole request: one service that has not started must not hide
            // the output of the ones that did -- which is usually where the
            // reason it did not start is written.
            for container in containers_of(&containers, name) {
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

                streams.push(crate::logs::lines(name.clone(), chunks).boxed());
                if services.last() != Some(name) {
                    services.push(name.clone());
                }
            }
        }

        Ok(Logs {
            services,
            lines: futures::stream::select_all(streams).boxed(),
        })
    }

    async fn exec(&self, deployment: &Deployment, options: &ExecOptions) -> Result<Exec, Error> {
        if options.command.is_empty() {
            return Err(Error::Invalid {
                reason: "no command was given to run".into(),
            });
        }
        if !deployment.services.contains_key(&options.service) {
            return Err(Error::NoSuchService {
                service: options.service.clone(),
            });
        }
        // A service that is not running is an error rather than an empty
        // stream: "the command produced no output" and "there was nothing to
        // run it in" are opposite answers to a debugging question.
        let containers = self.containers(&deployment.project).await?;
        let mut container = None;
        for candidate in containers_of(&containers, &options.service) {
            let running = self
                .inspect_service(&candidate)
                .await?
                .is_some_and(|s| s.status.is_running());
            if running || container.is_none() {
                container = Some(candidate);
            }
            if running {
                break;
            }
        }
        let Some(container) = container else {
            return Err(Error::NoSuchService {
                service: options.service.clone(),
            });
        };

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

        let started = self
            .docker
            .start_exec(&created.id, None)
            .await
            .map_err(Error::docker)?;
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
                Ok(ExecChunk::Exit {
                    code: status.exit_code,
                })
            }));

        Ok(Exec {
            container,
            output: body.boxed(),
        })
    }

    async fn stop(&self, deployment: &Deployment) -> Result<(), Error> {
        crate::compose::stop(&deployment.project).await
    }

    async fn remove(&self, deployment: &Deployment, remove_volumes: bool) -> Result<(), Error> {
        crate::compose::down(&deployment.project, remove_volumes).await
    }
}

/// The names of one service's containers.
fn containers_of(containers: &[ContainerSummary], service: &str) -> Vec<String> {
    containers
        .iter()
        .filter(|c| {
            c.labels
                .as_ref()
                .and_then(|l| l.get(LABEL_SERVICE))
                .is_some_and(|s| s == service)
        })
        .filter_map(|c| c.names.as_ref()?.first())
        .map(|n| n.trim_start_matches('/').to_string())
        .collect()
}

/// How much is wrong with a container, to pick which one speaks for a service.
fn trouble(state: &ServiceState) -> u8 {
    match (state.status, state.health) {
        (ContainerStatus::Dead, _) => 7,
        (ContainerStatus::Exited { code }, _) if code != 0 => 6,
        (ContainerStatus::Restarting, _) => 5,
        (_, HealthStatus::Unhealthy) => 4,
        (ContainerStatus::Paused | ContainerStatus::Created | ContainerStatus::Absent, _) => 3,
        (_, HealthStatus::Starting) => 2,
        (ContainerStatus::Exited { .. }, _) => 1,
        _ => 0,
    }
}
