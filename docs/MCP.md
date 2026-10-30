# MCP surface

viaos has no UI to start. Claude is the UI. Every operation is a typed
function first; MCP tools, the CLI, and any later UI call the same functions.

## Target prompts

These are the acceptance tests. A tool exists because one of these needs it.

| Prompt | Tools | Status |
| --- | --- | --- |
| "How's line-07 doing?" | `list_devices`, `device_health`, `logs`, `journal` | works |
| "Update all of fleet X to version Y" (OS) | `list_os_versions`, `update_os` (dry_run first) | works |
| "Update all of fleet X to version Y" (apps) | local `release_app`, then `list_releases`, `deploy` (dry_run first) | works, including rollback on a failed health gate |
| "Provision the attached device" | `create_provisioning_image`, then local `list_disks`, `flash_image` | works; not yet tried on a real card |
| "Make a provisioning image for a Pi and an x86 box on fleet X" | `create_provisioning_image` x2 | works: both enrolled into one fleet (Pi image booted in emulated arm64 UEFI; not yet on a real Pi) |

## Servers

| Server | Where | Tools |
| --- | --- | --- |
| device | `via-agent mcp` on the device (stdio) | `device_health`, `logs`, `journal`, `os_update`, `deploy` |
| fleet | `via-server` at `/mcp` (HTTP, bearer token) | `list_fleets`, `create_fleet`, `list_devices`, `device_health`, `logs`, `journal`, `list_os_versions`, `update_os`, `list_releases`, `deploy`, `create_provisioning_image` |
| local | `via mcp` on the operator's machine (stdio, root) | `list_disks`, `flash_image`, `flash_status`, `release_app` |

The fleet server forwards device tools over each device's outbound
connection, adding only a `device` argument, so a tool behaves the same
through the control plane as over `ssh device via-agent mcp`.

## Rules

1. Every tool carries MCP's read-only or destructive hint. Tools that change
   many devices take `dry_run` and return the plan when it is set.
2. Long work returns at once and reports progress where the caller already
   looks: an OS update under `os.update` in `device_health`, a flash in
   `flash_status`.
3. No tool returns a secret. Enrollment tokens are written straight into an
   image or onto a device; the tool returns a path or a device id, never the
   token.
4. Every fleet call is written to the audit log with its arguments and
   whether it succeeded.

## Provisioning without a rebuild

One generic image per architecture (`x86_64`, `aarch64`). Provisioning writes
`viaos.toml` (control plane URL, single-use enrollment token, hostname) onto
the image's FAT boot partition. `create_provisioning_image` copies the base
image, writes that one file and compresses the result: about 7 seconds, no
Nix on the server.
