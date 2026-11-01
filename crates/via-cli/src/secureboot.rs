//! UEFI Secure Boot: the operator's own keys, and signing what devices boot.
//!
//! `via secure-boot keygen <dir>` makes a PK, KEK and db. Their `.auth`
//! files go onto every base image's ESP under `loader/keys/auto/`, and
//! systemd-boot enrolls them the first time the device boots with its
//! firmware in setup mode. From then on the firmware runs only EFI binaries
//! signed with the db key: systemd-boot and the UKIs `via base-image
//! --secure-boot` and `via os-release --secure-boot` sign. The UKI carries
//! the kernel, initrd, command line and the verity root hash of /nix/store,
//! so the signature covers everything the device runs.
//!
//! The signing itself is sbsign's; the key lists are openssl's and efitools'.
//! The Nix package puts all three on `via`'s PATH.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

/// Where systemd-boot looks for keys to enroll.
const ESP_KEYS: &str = "loader/keys/auto";
const KEYS: [&str; 3] = ["PK", "KEK", "db"];

fn run(cmd: &mut Command) -> Result<()> {
    let out = cmd
        .output()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    ensure!(
        out.status.success(),
        "{:?} failed: {}",
        cmd.get_program(),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// Make a PK, KEK and db in `dir`, each as `.key`, `.crt` and the signed
/// `.auth` firmware enrolls.
pub fn keygen(dir: &Path) -> Result<Value> {
    if dir.join("db.key").exists() {
        bail!(
            "{} already has keys; refusing to overwrite them",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
    let owner = std::fs::read_to_string("/proc/sys/kernel/random/uuid")?
        .trim()
        .to_string();
    for name in KEYS {
        run(Command::new("openssl").current_dir(dir).args([
            "req",
            "-new",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-sha256",
            "-days",
            "7300",
            "-subj",
            &format!("/CN=viaos {name}/"),
            "-keyout",
            &format!("{name}.key"),
            "-out",
            &format!("{name}.crt"),
        ]))?;
        run(Command::new("cert-to-efi-sig-list").current_dir(dir).args([
            "-g",
            &owner,
            &format!("{name}.crt"),
            &format!("{name}.esl"),
        ]))?;
    }
    // Each list is signed by the key above it; PK signs itself.
    for (name, signer) in [("PK", "PK"), ("KEK", "PK"), ("db", "KEK")] {
        run(Command::new("sign-efi-sig-list").current_dir(dir).args([
            "-g",
            &owner,
            "-k",
            &format!("{signer}.key"),
            "-c",
            &format!("{signer}.crt"),
            name,
            &format!("{name}.esl"),
            &format!("{name}.auth"),
        ]))?;
    }
    Ok(json!({
        "dir": dir,
        "next": "Pass --secure-boot to `via base-image` and `via os-release`. Devices enroll these \
                 keys on first boot if their firmware is in setup mode. Keep the .key files \
                 off the server, like the update signing key.",
    }))
}

/// Sign the EFI binary at `path` in place with the db key.
pub fn sign(keys: &Path, path: &Path) -> Result<()> {
    let signed = path.with_extension("signed");
    run(Command::new("sbsign")
        .arg("--key")
        .arg(keys.join("db.key"))
        .arg("--cert")
        .arg(keys.join("db.crt"))
        .arg("--output")
        .arg(&signed)
        .arg(path))?;
    std::fs::rename(&signed, path)?;
    Ok(())
}

/// A copy of an OS update directory with its UKI signed and the manifest's
/// hashes redone. The caller removes it.
pub fn sign_update(keys: &Path, dir: &Path) -> Result<PathBuf> {
    use sha2::Digest;
    let out = std::env::temp_dir().join(format!("via-os-sb-{}", std::process::id()));
    std::fs::create_dir_all(&out)?;
    let mut manifest: Value = serde_json::from_slice(&std::fs::read(dir.join("manifest.json"))?)?;
    for f in manifest["files"]
        .as_array_mut()
        .context("manifest has no files")?
    {
        let name = f["name"]
            .as_str()
            .context("manifest file has no name")?
            .to_string();
        let path = out.join(&name);
        if !name.ends_with(".efi") {
            // The partition images are large and unchanged: link, don't copy.
            std::fs::hard_link(dir.join(&name), &path)
                .or_else(|_| std::os::unix::fs::symlink(dir.join(&name).canonicalize()?, &path))?;
            continue;
        }
        std::fs::copy(dir.join(&name), &path)?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o644))?;
        sign(keys, &path)?;
        let bytes = std::fs::read(&path)?;
        f["sha256"] = json!(hex::encode(sha2::Sha256::digest(&bytes)));
        f["size"] = json!(bytes.len());
    }
    manifest["secure_boot"] = json!(true);
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(out)
}

/// Sign every EFI binary on the image's boot partition, add the keys for
/// systemd-boot to enroll, and tell it to enroll them.
pub fn sign_image(keys: &Path, image: &Path) -> Result<()> {
    use std::io::{Read, Write};
    let fs = via_image::open_esp(image)?;
    let tmp = std::env::temp_dir().join(format!("via-sb-{}.efi", std::process::id()));

    let mut binaries = Vec::new();
    collect_efi(&fs.root_dir(), "", &mut binaries)?;
    ensure!(
        !binaries.is_empty(),
        "no EFI binaries on the boot partition"
    );
    for path in &binaries {
        let mut bytes = Vec::new();
        fs.root_dir().open_file(path)?.read_to_end(&mut bytes)?;
        std::fs::write(&tmp, &bytes)?;
        sign(keys, &tmp)?;
        let mut f = fs.root_dir().create_file(path)?;
        f.truncate()?;
        f.write_all(&std::fs::read(&tmp)?)?;
    }
    let _ = std::fs::remove_file(&tmp);

    let mut dir = fs.root_dir();
    for part in ESP_KEYS.split('/') {
        dir = dir.create_dir(part)?;
    }
    for name in KEYS {
        let mut f = dir.create_file(&format!("{name}.auth"))?;
        f.truncate()?;
        f.write_all(&std::fs::read(keys.join(format!("{name}.auth")))?)?;
    }

    // Enrolling only happens in setup mode, which someone chose by clearing
    // the firmware's keys, so it need not wait for a key press.
    let mut conf = String::new();
    fs.root_dir()
        .open_file("loader/loader.conf")?
        .read_to_string(&mut conf)?;
    if !conf.contains("secure-boot-enroll") {
        conf.push_str("secure-boot-enroll force\n");
    }
    let mut f = fs.root_dir().create_file("loader/loader.conf")?;
    f.truncate()?;
    f.write_all(conf.as_bytes())?;
    drop(f);
    drop(dir);
    fs.unmount()?;
    Ok(())
}

fn collect_efi(dir: &via_image::Dir<'_>, prefix: &str, out: &mut Vec<String>) -> Result<()> {
    for entry in dir.iter() {
        let entry = entry?;
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let path = format!("{prefix}{name}");
        if entry.is_dir() {
            collect_efi(&entry.to_dir(), &format!("{path}/"), out)?;
        } else if name.to_ascii_lowercase().ends_with(".efi") {
            out.push(path);
        }
    }
    Ok(())
}
