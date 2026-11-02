//! A terminal on the device, carried over its connection to the control
//! plane. The control plane's admins already have `exec` and SSH; this is the
//! same access for a person in a browser.
//!
//! Output is the terminal's bytes. Input is framed, because a resize has to
//! travel with the keystrokes:
//!
//! ```text
//! 0 <len: u16 BE> <bytes>        keystrokes
//! 1 <cols: u16 BE> <rows: u16 BE> window size
//! ```

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const PATH: &str = "/run/wrappers/bin:/run/current-system/sw/bin:/usr/bin:/bin";

fn set_size(fd: i32, cols: u16, rows: u16) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: fd is an open terminal and `size` outlives the call.
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size) };
}

fn open_pty() -> Result<(OwnedFd, OwnedFd)> {
    let (mut master, mut slave) = (0, 0);
    // SAFETY: the out pointers are valid; null means default name and modes.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()).context("openpty");
    }
    // SAFETY: openpty succeeded, so both are open and ours alone.
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
}

fn command(container: Option<&str>) -> Result<Command> {
    let mut cmd = match container {
        Some(name) => {
            // Names come from the operator; docker would take `-x` as a flag.
            let valid = !name.is_empty()
                && !name.starts_with('-')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
            if !valid {
                bail!("bad container name {name:?}");
            }
            let mut c = Command::new("docker");
            c.args(["exec", "-it", name, "sh", "-c"]);
            c.arg("command -v bash >/dev/null && exec bash || exec sh");
            c
        }
        None => {
            let mut c = Command::new("bash");
            c.arg("-l");
            c
        }
    };
    cmd.env("TERM", "xterm-256color").env("PATH", PATH);
    if std::fs::read_dir("/root").is_ok() {
        cmd.env("HOME", "/root").current_dir("/root");
    }
    Ok(cmd)
}

/// Run a shell until either end goes away.
pub async fn serve(container: Option<String>, stream: DuplexStream) -> Result<()> {
    let (master, slave) = open_pty()?;
    let mut cmd = command(container.as_deref())?;
    let slave_fd = slave.as_raw_fd();
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: only async-signal-safe calls, between fork and exec. The child
    // starts its own session with the terminal as its controlling one.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = tokio::process::Command::from(cmd)
        .kill_on_drop(true)
        .spawn()
        .context("starting the shell")?;
    drop(slave);

    let master_fd = master.as_raw_fd();
    let write_end = master.try_clone()?;
    let mut from_pty = tokio::fs::File::from_std(std::fs::File::from(master));
    let mut to_pty = tokio::fs::File::from_std(std::fs::File::from(write_end));
    let (mut r, mut w) = tokio::io::split(stream);

    let input = async {
        loop {
            match r.read_u8().await {
                Ok(0) => {
                    let len = r.read_u16().await? as usize;
                    let mut buf = vec![0; len];
                    r.read_exact(&mut buf).await?;
                    to_pty.write_all(&buf).await?;
                    to_pty.flush().await?;
                }
                Ok(1) => {
                    let (cols, rows) = (r.read_u16().await?, r.read_u16().await?);
                    set_size(master_fd, cols, rows);
                }
                Ok(other) => bail!("bad shell frame {other}"),
                Err(_) => return Ok(()),
            }
        }
    };
    let output = async {
        let mut buf = vec![0u8; 16 * 1024];
        // A read error is how a terminal says its shell exited.
        while let Ok(n) = from_pty.read(&mut buf).await {
            if n == 0 || w.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
        Ok::<(), anyhow::Error>(())
    };
    let result: Result<()> = tokio::select! {
        r = input => r,
        r = output => r,
        _ = child.wait() => Ok(()),
    };
    // Leaving the terminal open would keep a blocking read parked forever.
    drop(child);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_names_cannot_be_flags() {
        assert!(command(Some("hello-web-1")).is_ok());
        assert!(command(Some("--privileged")).is_err());
        assert!(command(Some("a b")).is_err());
        assert!(command(Some("")).is_err());
    }

    #[tokio::test]
    async fn runs_a_command_and_returns_its_output() {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        tokio::spawn(serve(None, theirs));
        let (mut r, mut w) = tokio::io::split(ours);
        let line = b"echo via-$((40+2)); exit\n";
        w.write_all(&[1, 0, 80, 0, 24]).await.unwrap();
        w.write_all(&[0, 0, line.len() as u8]).await.unwrap();
        w.write_all(line).await.unwrap();
        let mut out = Vec::new();
        let read = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            let mut buf = [0u8; 4096];
            while let Ok(n) = r.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&out).contains("via-42") {
                    break;
                }
            }
        });
        read.await.unwrap();
        assert!(String::from_utf8_lossy(&out).contains("via-42"));
    }
}
