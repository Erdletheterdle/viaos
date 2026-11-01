//! Deciding whether this boot is good.
//!
//! A new OS version boots on trial: systemd-boot counts its tries, and only
//! reaching boot-complete.target marks it good. `via-agent ready` is what
//! boot-complete.target waits for, so "good" means the device can do its
//! job: Docker answers, and the agent reached the control plane. Reaching it
//! is the check that matters most; an update that breaks networking leaves
//! a device nobody can fix remotely.
//!
//! When a trial boot is not good in time, waiting out the remaining tries
//! would take a reboot each that nothing triggers. Instead the entry is
//! marked bad at once and the device reboots into the version it came from.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use via_runtime::{DockerRuntime, Runtime};

pub async fn run(control: bool, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut waiting_for;
    loop {
        waiting_for = missing(control).await;
        if waiting_for.is_none() || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let Some(what) = waiting_for else {
        tracing::info!("boot is good");
        return Ok(());
    };
    tracing::error!("boot is not good after {}s: {what}", timeout.as_secs());
    if bless_status().await.as_deref() == Some("indeterminate") {
        tracing::error!("this is a trial boot of a new version: marking it bad and rebooting");
        bless("bad").await;
        let _ = tokio::process::Command::new("systemctl")
            .arg("reboot")
            .status()
            .await;
    }
    bail!("{what}")
}

async fn missing(control: bool) -> Option<&'static str> {
    let docker = match DockerRuntime::connect() {
        Ok(rt) => rt.info().await.is_ok(),
        Err(_) => false,
    };
    if !docker {
        return Some("Docker does not answer");
    }
    if control && !std::path::Path::new(crate::link::CONNECTED).exists() {
        return Some("the control plane has not been reached");
    }
    None
}

fn bless_tool() -> String {
    std::env::var("VIA_BLESS_BOOT").unwrap_or_else(|_| "systemd-bless-boot".into())
}

async fn bless_status() -> Option<String> {
    let out = tokio::process::Command::new(bless_tool())
        .arg("status")
        .output()
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn bless(verdict: &str) {
    let _ = tokio::process::Command::new(bless_tool())
        .arg(verdict)
        .status()
        .await;
}
