//! What the device has been using lately. A sampler keeps the last hour in
//! memory, so a page can show a history the moment it opens and nothing is
//! written to the disk for it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use via_core::{Deployment, Mount};
use via_deploy::{Deployer, Store, SystemClock};
use via_runtime::{DiskUsage, DockerRuntime};

use crate::host;

pub const INTERVAL_SECS: u64 = 5;
/// An hour's worth.
const KEEP: usize = 720;
/// Asking Docker for sizes can walk every volume.
const DISK_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize)]
pub struct ServiceSample {
    pub memory_mb: f32,
    pub cpu_percent: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub at: u64,
    pub memory_mb: u32,
    pub cpu_percent: f32,
    pub services: BTreeMap<String, ServiceSample>,
}

#[derive(Clone, Default)]
pub struct Metrics {
    samples: Arc<Mutex<VecDeque<Sample>>>,
    disk: Arc<Mutex<Option<(Instant, DiskUsage)>>>,
    refreshing: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
}

/// Percent of the machine's CPU used between two readings of the same counters.
fn cpu_percent(busy_before: u64, busy_after: u64, all_before: u64, all_after: u64) -> f32 {
    let all = all_after.saturating_sub(all_before);
    if all == 0 {
        return 0.0;
    }
    let busy = busy_after.saturating_sub(busy_before);
    ((busy as f64 / all as f64 * 1000.0).round() / 10.0).min(100.0) as f32
}

fn deployment(data_dir: &PathBuf, rt: &DockerRuntime) -> Option<Deployment> {
    let store = Store::open(data_dir).ok()?;
    let current = store.load_state().ok()?.current?;
    Deployer::new(&store, rt, &SystemClock)
        .load_deployment(current)
        .ok()
}

impl Metrics {
    /// Start sampling, once. Runs for as long as the process does.
    pub fn start(&self, data_dir: PathBuf) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            let mut host_cpu = host::cpu_jiffies();
            let mut last: HashMap<String, (u64, u64)> = HashMap::new();
            let mut tick = tokio::time::interval(Duration::from_secs(INTERVAL_SECS));
            loop {
                tick.tick().await;
                this.sample(&data_dir, &mut host_cpu, &mut last).await;
            }
        });
    }

    async fn sample(
        &self,
        data_dir: &PathBuf,
        host_cpu: &mut Option<(u64, u64)>,
        last: &mut HashMap<String, (u64, u64)>,
    ) {
        let Some((memory_mb, _)) = host::memory_mb() else {
            return;
        };
        let now_cpu = host::cpu_jiffies();
        let cpu = match (*host_cpu, now_cpu) {
            (Some((all0, idle0)), Some((all1, idle1))) => {
                cpu_percent(all0 - idle0, all1 - idle1, all0, all1)
            }
            _ => 0.0,
        };
        *host_cpu = now_cpu;

        let mut services = BTreeMap::new();
        if let Ok(rt) = DockerRuntime::connect()
            && let Some(d) = deployment(data_dir, &rt)
            && let Ok(stats) = rt.service_stats(&d).await
        {
            for s in stats {
                // Docker's system counter covers every CPU, so this is a share
                // of the whole machine, like the host's own figure.
                let cpu_percent = match last.get(&s.service) {
                    Some(&(cpu0, sys0)) => cpu_percent(cpu0, s.cpu_total_ns, sys0, s.system_cpu_ns),
                    None => 0.0,
                };
                last.insert(s.service.clone(), (s.cpu_total_ns, s.system_cpu_ns));
                services.insert(
                    s.service,
                    ServiceSample {
                        memory_mb: (s.memory_bytes as f64 / 104_857.6).round() as f32 / 10.0,
                        cpu_percent,
                    },
                );
            }
        }
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut samples = self.samples.lock().unwrap();
        samples.push_back(Sample {
            at,
            memory_mb: memory_mb as u32,
            cpu_percent: cpu,
            services,
        });
        while samples.len() > KEEP {
            samples.pop_front();
        }
    }

    /// Refresh Docker's sizes in the background if they're old. Whoever asked
    /// gets what's known now and doesn't wait on a slow disk.
    fn refresh_disk(&self) {
        let fresh = self
            .disk
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() < DISK_EVERY);
        if fresh || self.refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let this = self.clone();
        tokio::spawn(async move {
            if let Ok(rt) = DockerRuntime::connect() {
                match rt.disk_usage().await {
                    Ok(usage) => *this.disk.lock().unwrap() = Some((Instant::now(), usage)),
                    Err(e) => tracing::warn!("reading Docker's disk use failed: {e}"),
                }
            }
            this.refreshing.store(false, Ordering::SeqCst);
        });
    }

    /// Everything a page needs, with history from `since` (unix seconds) on.
    pub fn report(&self, data_dir: PathBuf, since: Option<u64>) -> Value {
        self.start(data_dir.clone());
        self.refresh_disk();
        let samples: Vec<Sample> = self
            .samples
            .lock()
            .unwrap()
            .iter()
            .filter(|s| since.is_none_or(|t| s.at > t))
            .cloned()
            .collect();
        let d = DockerRuntime::connect()
            .ok()
            .and_then(|rt| deployment(&data_dir, &rt));
        let disk = self.disk.lock().unwrap().as_ref().map(|(_, u)| u.clone());
        json!({
            "interval_secs": INTERVAL_SECS,
            "memory_total_mb": host::memory_mb().map(|(_, total)| total),
            "samples": samples,
            "processes": host::top_processes(8),
            "disk": host::whole_disk(),
            "docker": disk.as_ref().map(|u| json!({
                "images_mb": u.images_bytes / 1_048_576,
                "containers_mb": u.containers_bytes / 1_048_576,
                "volumes_mb": u.volumes_bytes / 1_048_576,
                "build_cache_mb": u.build_cache_bytes / 1_048_576,
            })),
            "volumes": disk.as_ref().map(|u| volumes(d.as_ref(), u)),
        })
    }
}

/// The volumes the running release declares, each with its size, then any
/// others Docker is keeping, such as those a past release left behind.
fn volumes(d: Option<&Deployment>, usage: &DiskUsage) -> Vec<Value> {
    let size = |name: &str| usage.volumes.iter().find(|v| v.name == name);
    let mut out = Vec::new();
    let mut known = Vec::new();
    if let Some(d) = d {
        for declared in d.volumes.keys() {
            let name = d.volume_name(declared);
            let used_by: Vec<&String> = d
                .services
                .iter()
                .filter(|(_, s)| {
                    s.mounts
                        .iter()
                        .any(|m| matches!(m, Mount::Volume { source, .. } if source == declared))
                })
                .map(|(service, _)| service)
                .collect();
            let found = size(&name);
            out.push(json!({
                "name": declared,
                "declared": true,
                "used_by": used_by,
                "mb": found.map(|v| v.bytes as f64 / 1_048_576.0),
            }));
            known.push(name);
        }
    }
    for v in usage.volumes.iter().filter(|v| !known.contains(&v.name)) {
        out.push(json!({
            "name": v.name,
            "declared": false,
            "used_by": [],
            "containers": v.containers,
            "mb": v.bytes as f64 / 1_048_576.0,
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use via_core::{ImageRef, Platform, Service, Volume};
    use via_runtime::VolumeUsage;

    #[test]
    fn cpu_is_the_share_between_two_readings() {
        assert_eq!(cpu_percent(100, 150, 1000, 1100), 50.0);
        assert_eq!(cpu_percent(100, 100, 1000, 1000), 0.0, "no time passed");
        assert_eq!(
            cpu_percent(100, 90, 1000, 1100),
            0.0,
            "a container restarted"
        );
        assert_eq!(cpu_percent(0, 500, 0, 100), 100.0);
    }

    #[test]
    fn declared_volumes_come_first_and_leftovers_are_marked() {
        let mut d = Deployment::new("hello", Platform::new("linux", "amd64"));
        d.volumes.insert("data".into(), Volume::default());
        d.volumes.insert("fresh".into(), Volume::default());
        let mut web = Service::new("web", ImageRef::new("web:1"));
        web.mounts.push(Mount::Volume {
            source: "data".into(),
            target: "/data".into(),
            read_only: false,
        });
        d.services.insert("web".into(), web);
        d.services.insert(
            "worker".into(),
            Service::new("worker", ImageRef::new("w:1")),
        );
        let usage = DiskUsage {
            volumes: vec![
                VolumeUsage {
                    name: "hello_data".into(),
                    bytes: 3 * 1_048_576,
                    containers: 1,
                },
                VolumeUsage {
                    name: "old_cache".into(),
                    bytes: 1_048_576,
                    containers: 0,
                },
            ],
            ..DiskUsage::default()
        };
        let v = volumes(Some(&d), &usage);
        assert_eq!(v[0]["name"], "data");
        assert_eq!(v[0]["used_by"], json!(["web"]));
        assert_eq!(v[0]["mb"], 3.0);
        assert_eq!(v[1]["name"], "fresh");
        assert!(v[1]["mb"].is_null(), "not created yet");
        assert_eq!(v[2]["name"], "old_cache");
        assert_eq!(v[2]["declared"], false);
    }
}
