# MCP surface

viaos has no UI to start. Claude is the UI. Every operation is a typed
function first; MCP tools, the CLI, and any later UI call the same functions.

## Target prompts

These are the acceptance tests. A tool exists because one of these needs it.

| Prompt | Tools |
| --- | --- |
| "How's line-07 doing?" | `device_health`, `containers`, `logs` |
| "Update all of fleet X to version Y" | `fleet_status`, `deploy_fleet` (dry_run first) |
| "Provision the attached device" | `discover_devices`, `provision_device` |
| "Make a provisioning image for a Pi and an x86 box on fleet X" | `build_provisioning_image` |

## Two servers, one tool set

- **Device** (`via-agent mcp`): stdio, reached by `ssh host via-agent mcp` or
  the local socket. Works with no control plane.
- **Fleet** (`via-server`): the device tools plus a `device` argument,
  forwarded over each device's outbound connection, plus fleet-only tools.

## Rules

1. Every tool is tagged `read` or `mutate`. Mutating tools take `dry_run` and
   return the plan when it is set.
2. Long work returns a job id; `job_status` polls it.
3. No tool returns a secret. Enrollment tokens are written straight into an
   image or onto a device; the tool returns a path or a device id, never the
   token.
4. Every call is written to the audit log with caller, args, and result.

## Provisioning without a rebuild

One generic image per architecture (`x86_64`, `aarch64`). Provisioning writes
`viaos.toml` (fleet URL, enrollment token, hostname) onto the image's FAT boot
partition. `build_provisioning_image` is "copy base image, write one file";
it needs no Nix on the server and takes seconds.
