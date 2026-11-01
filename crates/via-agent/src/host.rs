//! Host vitals read straight from procfs and sysfs.
//!
//! No metrics crate: the agent needs a dozen numbers, and every one of them is
//! a line in a file the kernel already keeps current.

use std::ffi::CString;
use std::fs;

use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Host {
    pub hostname: String,
    pub os: Option<String>,
    pub kernel: Option<String>,
    pub uptime_secs: Option<u64>,
    pub load: Option<[f64; 3]>,
    pub cpus: usize,
    pub memory: Option<Memory>,
    pub disks: Vec<Disk>,
    pub temperatures_c: Vec<Temperature>,
}

#[derive(Debug, Serialize)]
pub struct Memory {
    pub total_mb: u64,
    pub available_mb: u64,
    pub used_percent: f64,
}

#[derive(Debug, Serialize)]
pub struct Disk {
    pub mount: String,
    pub total_mb: u64,
    pub free_mb: u64,
    pub used_percent: f64,
}

#[derive(Debug, Serialize)]
pub struct Temperature {
    pub zone: String,
    pub celsius: f64,
}

/// Mounts worth reporting. `/var/lib/docker` is where a full disk actually
/// stops a device; `/` is read-only on viaos but still worth seeing.
const MOUNTS: &[&str] = &["/", "/var", "/var/lib/docker", "/boot"];

pub fn read() -> Host {
    Host {
        hostname: read_trim("/proc/sys/kernel/hostname").unwrap_or_default(),
        os: os_release(),
        kernel: read_trim("/proc/sys/kernel/osrelease"),
        uptime_secs: read_trim("/proc/uptime")
            .and_then(|s| s.split_whitespace().next()?.parse::<f64>().ok())
            .map(|s| s as u64),
        load: read_trim("/proc/loadavg").and_then(|s| {
            let mut it = s.split_whitespace().map(|v| v.parse::<f64>().ok());
            Some([it.next()??, it.next()??, it.next()??])
        }),
        cpus: std::thread::available_parallelism().map_or(1, |n| n.get()),
        memory: memory(),
        disks: disks(),
        temperatures_c: temperatures(),
    }
}

fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn os_release() -> Option<String> {
    let text = fs::read_to_string("/etc/os-release").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string())
}

fn memory() -> Option<Memory> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    let kb = |key: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(key))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    };
    let total = kb("MemTotal:")?;
    let available = kb("MemAvailable:")?;
    Some(Memory {
        total_mb: total / 1024,
        available_mb: available / 1024,
        used_percent: percent(total - available, total),
    })
}

fn disks() -> Vec<Disk> {
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for mount in MOUNTS {
        let Some((dev, total, free)) = statvfs(mount) else {
            continue;
        };
        // A path that is not its own mount reports its parent's filesystem;
        // listing it twice would only make a full disk look like two.
        if seen.contains(&dev) {
            continue;
        }
        seen.push(dev);
        out.push(Disk {
            mount: mount.to_string(),
            total_mb: total / 1_048_576,
            free_mb: free / 1_048_576,
            used_percent: percent(total - free, total),
        });
    }
    out
}

fn statvfs(path: &str) -> Option<(u64, u64, u64)> {
    let c = CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let frsize = st.f_frsize as u64;
    Some((
        st.f_fsid as u64,
        st.f_blocks as u64 * frsize,
        st.f_bavail as u64 * frsize,
    ))
}

fn temperatures() -> Vec<Temperature> {
    let Ok(entries) = fs::read_dir("/sys/class/thermal") else {
        return Vec::new();
    };
    let mut out: Vec<Temperature> = entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("thermal_zone"))
        .filter_map(|e| {
            let p = e.path();
            let milli: f64 = read_trim(p.join("temp").to_str()?)?.parse().ok()?;
            let zone = read_trim(p.join("type").to_str()?).unwrap_or_default();
            Some(Temperature {
                zone,
                celsius: milli / 1000.0,
            })
        })
        .collect();
    out.sort_by(|a, b| a.zone.cmp(&b.zone));
    out
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    (part as f64 * 1000.0 / whole as f64).round() / 10.0
}
