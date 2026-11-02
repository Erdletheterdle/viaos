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

/// The disk the system lives on and every partition of it, including the
/// ones nothing here mounts directly: the A/B OS slots.
#[derive(Debug, Serialize)]
pub struct WholeDisk {
    pub device: String,
    pub total_mb: u64,
    /// Space no partition claims.
    pub unallocated_mb: u64,
    pub partitions: Vec<Partition>,
}

#[derive(Debug, Serialize)]
pub struct Partition {
    pub device: String,
    pub label: Option<String>,
    pub size_mb: u64,
    /// Where it's mounted, or the mapped device using it such as `usr`.
    pub in_use_as: Option<String>,
    /// Only for what is mounted here: how much of it is free.
    pub free_mb: Option<u64>,
}

fn sectors_mb(path: &std::path::Path) -> Option<u64> {
    let sectors: u64 = fs::read_to_string(path).ok()?.trim().parse().ok()?;
    Some(sectors * 512 / 1_048_576)
}

pub fn whole_disk() -> Option<WholeDisk> {
    let mounts = fs::read_to_string("/proc/self/mounts").ok()?;
    // /var on viaos, where / is a tmpfs; / on anything else this runs on.
    let source = ["/var", "/"].iter().find_map(|wanted| {
        mounts.lines().find_map(|l| {
            let mut f = l.split_whitespace();
            let (dev, mount) = (f.next()?, f.next()?);
            (mount == *wanted && dev.starts_with("/dev/")).then(|| dev.to_string())
        })
    })?;
    let part = source.strip_prefix("/dev/")?;
    // /sys/class/block/vda6 links to .../block/vda/vda6.
    let real = fs::read_link(format!("/sys/class/block/{part}")).ok()?;
    let disk = real.parent()?.file_name()?.to_str()?.to_string();
    let root = std::path::PathBuf::from("/sys/block").join(&disk);
    let total_mb = sectors_mb(&root.join("size"))?;

    let mut labels = std::collections::HashMap::new();
    for e in fs::read_dir("/dev/disk/by-partlabel")
        .into_iter()
        .flatten()
        .flatten()
    {
        if let (Ok(target), Some(label)) = (fs::read_link(e.path()), e.file_name().to_str()) {
            if let Some(dev) = target.file_name().and_then(|n| n.to_str()) {
                labels.insert(dev.to_string(), decode_label(label));
            }
        }
    }

    let mut partitions = Vec::new();
    for e in fs::read_dir(&root).ok()?.flatten() {
        if !e.path().join("partition").exists() {
            continue;
        }
        let device = e.file_name().to_string_lossy().into_owned();
        let size_mb = sectors_mb(&e.path().join("size")).unwrap_or(0);
        let mount = mounts.lines().find_map(|l| {
            let mut f = l.split_whitespace();
            let (dev, mount) = (f.next()?, f.next()?);
            (dev == format!("/dev/{device}")).then(|| mount.to_string())
        });
        let free_mb = mount
            .as_deref()
            .and_then(statvfs)
            .map(|(_, _, free)| free / 1_048_576);
        // Not mounted itself, but held by something like a verity mapping.
        let held = fs::read_dir(e.path().join("holders"))
            .into_iter()
            .flatten()
            .flatten()
            .find_map(|h| {
                fs::read_to_string(h.path().join("dm/name"))
                    .ok()
                    .map(|n| n.trim().to_string())
            });
        partitions.push(Partition {
            label: labels.get(&device).cloned(),
            in_use_as: mount.or(held),
            free_mb,
            size_mb,
            device,
        });
    }
    partitions.sort_by_key(|p| natural(&p.device));
    let allocated: u64 = partitions.iter().map(|p| p.size_mb).sum();
    Some(WholeDisk {
        device: disk,
        total_mb,
        unallocated_mb: total_mb.saturating_sub(allocated),
        partitions,
    })
}

/// Udev escapes characters in link names as `\\x2d`.
fn decode_label(link: &str) -> String {
    let mut out = String::new();
    let mut rest = link;
    while let Some(i) = rest.find("\\x") {
        out.push_str(&rest[..i]);
        let hex = rest
            .get(i + 2..i + 4)
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match hex {
            Some(b) => {
                out.push(b as char);
                rest = &rest[i + 4..];
            }
            None => {
                out.push_str("\\x");
                rest = &rest[i + 2..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `vda10` after `vda9`.
fn natural(device: &str) -> (usize, String) {
    (device.len(), device.to_string())
}

/// Memory by program, the biggest first. Threads aren't counted twice, and
/// kernel threads hold nothing here.
#[derive(Debug, Serialize)]
pub struct ProcessMemory {
    pub name: String,
    pub processes: u32,
    pub mb: f64,
}

pub fn top_processes(limit: usize) -> Vec<ProcessMemory> {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let mut by_name: std::collections::HashMap<String, (u32, u64)> = Default::default();
    for e in fs::read_dir("/proc").into_iter().flatten().flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Some(rss) = fs::read_to_string(e.path().join("statm"))
            .ok()
            .and_then(|s| parse_rss(&s))
        else {
            continue;
        };
        if rss == 0 {
            continue;
        }
        let comm = fs::read_to_string(e.path().join("comm")).unwrap_or_default();
        let entry = by_name.entry(comm.trim().to_string()).or_default();
        entry.0 += 1;
        entry.1 += rss * page;
    }
    let mut out: Vec<ProcessMemory> = by_name
        .into_iter()
        .map(|(name, (processes, bytes))| ProcessMemory {
            name,
            processes,
            mb: (bytes as f64 / 104_857.6).round() / 10.0,
        })
        .collect();
    out.sort_by(|a, b| b.mb.total_cmp(&a.mb));
    out.truncate(limit);
    out
}

/// The second field of `/proc/<pid>/statm`, in pages.
fn parse_rss(statm: &str) -> Option<u64> {
    statm.split_whitespace().nth(1)?.parse().ok()
}

/// Memory in use, in MB, and the whole.
pub fn memory_mb() -> Option<(u64, u64)> {
    let m = memory()?;
    Some((m.total_mb - m.available_mb, m.total_mb))
}

/// Total and idle jiffies across all CPUs, for comparing with a later reading.
pub fn cpu_jiffies() -> Option<(u64, u64)> {
    parse_cpu(fs::read_to_string("/proc/stat").ok()?.lines().next()?)
}

fn parse_cpu(line: &str) -> Option<(u64, u64)> {
    let v: Vec<u64> = line
        .strip_prefix("cpu")?
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    let idle = v.get(3)? + v.get(4).unwrap_or(&0);
    Some((v.iter().sum(), idle))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_what_proc_says() {
        assert_eq!(parse_rss("1000 250 30 4 0 50 0"), Some(250));
        assert_eq!(parse_rss("nope"), None);
        assert_eq!(parse_cpu("cpu  10 0 5 80 5 0 0 0 0 0"), Some((100, 85)));
        assert_eq!(parse_cpu("intr 1"), None);
    }

    #[test]
    fn partition_labels_are_unescaped() {
        assert_eq!(decode_label("viaos_1"), "viaos_1");
        assert_eq!(decode_label("usr\\x2dx86"), "usr-x86");
        assert_eq!(decode_label("odd\\xzz"), "odd\\xzz");
    }

    #[test]
    fn partitions_sort_as_numbers() {
        let mut v = ["vda10", "vda2", "vda1"];
        v.sort_by_key(|d| natural(d));
        assert_eq!(v, ["vda1", "vda2", "vda10"]);
    }

    #[test]
    fn this_machine_reads_without_falling_over() {
        assert!(!top_processes(5).is_empty());
        assert!(memory_mb().is_some());
    }
}
