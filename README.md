# viaos

An open, immutable Linux for edge devices: NixOS-built, real Docker, A/B
updates, and an optional outbound control plane for devices behind NAT.
Managed through MCP first -- ask Claude how a device is doing.

```bash
cargo build -p via-agent
sudo ./target/debug/via-agent health      # JSON snapshot
claude mcp add viaos -- ssh <device> via-agent mcp
```

See [`docs/MCP.md`](docs/MCP.md) for the tool surface.

The `via-core`, `via-compose`, `via-bundle`, `via-runtime` and `via-deploy`
crates come from [beigesys/via](https://github.com/beigesys/via).
