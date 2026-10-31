# viaos

An open, immutable Linux for edge devices: NixOS-built, upstream Docker, A/B
OS updates with automatic rollback, and an optional control plane that
devices dial out to, so they can sit behind NAT. There is no web UI. Claude
is the UI, through MCP.

```text
 Claude ──MCP──▶ via-server ◀──WebSocket (outbound)── via-agent ── dockerd
   │                 │                                    │
   └──MCP──▶ via (on your laptop: flash SD cards)     viaos image
```

## Try it

```bash
# 1. Build the images (x86_64 here; aarch64 for Raspberry Pi 4)
nix build .#images.x86_64 -o result-x86

# 2. Run a control plane
mkdir -p data/base && ln -s $(readlink -f result-x86)/viaos_1.raw data/base/viaos-x86_64.raw
VIA_ADMIN_TOKEN=$(openssl rand -hex 16) VIA_PUBLIC_URL=https://via.example.com \
  VIA_DATA_DIR=data cargo run -p via-server

# 3. Point Claude at it
claude mcp add --transport http via https://via.example.com/mcp \
  --header "Authorization: Bearer $VIA_ADMIN_TOKEN"
claude mcp add via-local -- sudo via mcp     # for flashing attached disks
```

Then ask:

- "Make a provisioning image for a Pi and an x86 box on fleet `line`."
- "Flash it to the SD card I just plugged in."
- "How's line-07 doing?"
- "Update all of fleet `line` to version 2."

A device without a control plane is still fully manageable:
`claude mcp add line-07 -- ssh root@line-07 via-agent mcp`.

## Pieces

| Path | What |
| --- | --- |
| `nix/viaos.nix` | The OS: verity-protected A/B `/nix/store`, tmpfs `/`, `/var` grown on first boot |
| `nix/sysupdate.nix`, `nix/update.nix` | OS update files and how sysupdate installs them |
| `nix/firmware.nix` | The firmware shipped: common Ethernet, Wi-Fi and Bluetooth chips; `viaos.firmware` adds more |
| `nix/rpi.nix` | Raspberry Pi 4 / 400 / CM4 boot chain (firmware → U-Boot → systemd-boot) |
| `crates/via-agent` | Device daemon: applies `viaos.toml`, holds the control-plane link, serves device MCP |
| `crates/via-server` | Control plane: registry, enrollment, provisioning images, OS updates, fleet MCP |
| `crates/via-link` | Byte streams multiplexed over the device's one WebSocket |
| `crates/via-cli` | `via mcp` on the operator's machine: write images to SD cards and USB disks |
| `crates/via-{core,compose,bundle,runtime,deploy}` | App releases, from [beigesys/via](https://github.com/beigesys/via) |

The OS is about 1.1 GB installed. An OS update downloads about 610 MB, and
the raw image is 4.7 GB (two 2 GB store slots), so it needs an 8 GB disk.

`viaos.toml` on the boot partition configures a device (hostname, control
plane, SSH keys); see `nix/viaos.example.toml`. `docs/MCP.md` lists the tools.

## Development

```bash
cargo test --workspace
nix build .#images.x86_64-debug        # adds a root shell on the serial console
scripts/qemu.sh result/viaos_1.raw     # boot it; SSH on localhost:2222
```
