# Running a control plane

The control plane is one binary with one SQLite file. It needs a public
HTTPS address that devices can reach. Devices dial out to it, so they can
be behind NAT; the server cannot be.

## 1. Run it

On NixOS, with Caddy getting the certificate:

```nix
{
  inputs.viaos.url = "github:beigesys/viaos";
  outputs = { nixpkgs, viaos, ... }: {
    nixosConfigurations.via = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        viaos.nixosModules.via-server
        {
          services.via-server = {
            enable = true;
            domain = "via.example.com";
            trustedKeys = [ "<public key from `via keygen`>" ];
          };
        }
      ];
    };
  };
}
```

Anywhere else, as a container behind any TLS proxy:

```bash
nix build .#via-server-image && docker load < result
docker run -d --name via -p 127.0.0.1:8080:8080 -v via:/data \
  -e VIA_PUBLIC_URL=https://via.example.com \
  -e VIA_TRUSTED_KEYS=<public key> \
  via-server:latest
```

A matching Caddyfile:

```text
via.example.com {
  reverse_proxy 127.0.0.1:8080
}
```

The proxy must pass WebSockets through, which Caddy does by default.
Uploads are gigabytes, so it must not cap request bodies.

## 2. Make yourself a token

```bash
sudo -u via-server via-server token create ana            # NixOS
docker exec via via-server token create ana               # container
```

That prints a token once. Roles are `admin` (the default) and `viewer`
(`--role viewer`): viewers get the read-only tools only. `token list` shows
when each was last used, and `token revoke <name>` revokes one at once.
Every tool call is audited under the token's name.

## 3. Sign what devices run

```bash
via keygen ~/.config/via/signing.key    # writes signing.key and signing.key.pub
```

Put the `.pub` contents in `trustedKeys` / `VIA_TRUSTED_KEYS`. Every
provisioning image made from then on writes that key into the device's
`viaos.toml`. Those devices then refuse OS updates and app releases that
are not signed by it. Keep the private key off the server: it belongs in
CI or on the machine you release from.

## 4. Secure Boot (optional)

```bash
via secure-boot keygen ~/.config/via/secure-boot
export VIA_SECURE_BOOT_KEYS=~/.config/via/secure-boot
```

With that set, `via base-image` signs systemd-boot and the UKI on the
image and puts the keys on its boot partition; `via os-release` signs each
update's UKI. A device whose firmware is in setup mode (clear the keys in
its firmware menu once) enrolls them on first boot, reboots, and from then
on runs only what these keys signed. The UKI holds the kernel, initrd,
command line and the root hash of the dm-verity /nix/store, so that is the
whole OS. `device_status` shows `secure_boot`, and a device enforcing it
refuses an OS update that was not signed for it.

The keys replace the firmware's own, Microsoft's included. Hardware whose
graphics or network card loads a Microsoft-signed option ROM at boot will
not show a picture or PXE boot with only these keys. Keep the `.key` files
off the server.

## 5. Give it an OS

```bash
export VIA_SERVER=https://via.example.com VIA_TOKEN=<token>
export VIA_SIGNING_KEY=~/.config/via/signing.key
nix build .#images.x86_64 -o image && via base-image image/viaos_1.raw --arch x86_64
nix build .#images.aarch64 -o image-arm && via base-image image-arm/viaos_1.raw --arch aarch64
```

For each new OS version, bump `VERSION`, then run
`nix build .#updates.x86_64 -o update` and `via os-release update`.
`.github/workflows/release.yml` does both architectures on a `v<N>` tag.

## 6. Point Claude at it

```bash
claude mcp add --transport http via https://via.example.com/mcp \
  --header "Authorization: Bearer $VIA_TOKEN"
claude mcp add via-local -- sudo -E via mcp    # flashing, releases
```

## Variables and secrets

A release holds no secrets. In the compose file, a bare name says a service
needs a value the release does not carry:

```yaml
services:
  web:
    environment:
      - API_KEY          # supplied per fleet or device
      - LOG_LEVEL=info   # baked into the release
```

Set values on the console's Variables page (or with `set_variable`), for a
whole fleet or one device; a device's value beats its fleet's. Online devices
re-apply at once, and only services that listed the name are recreated.
Offline devices catch up when they reconnect. A service never receives a name
it did not list, and a device page warns when a running release is missing one.

Mark a value secret and it is encrypted on the server and never shown again.
The key is `secret.key` in the data directory, or `VIA_SECRET_KEY`: back it up
separately from the database, because without it the secrets are gone, and
with it a stolen backup is readable. Devices receive plaintext over their TLS
connection and keep it readable by root only in `/var/lib/via/env.json`, so a
release restarts, rolls back and recovers with no network. Don't give Claude a
real secret: it would pass through the conversation. Use the console.

## The web console

It is the server's own address, with nothing more to install. Sign in with an
API token; viewers can look but not change anything, and only admins get
terminals. It calls the same tools Claude does, so the audit log covers both.

To work on it:

```bash
cd web && bun install
VIA_SERVER=http://localhost:8471 bun run dev
```

`bun run build` and a rebuild of `via-server` embed it. After changing
`web/bun.lock`, update the hash in `nix/web.nix`.

## Reaching a device

Devices are behind NAT, so everything goes over their own connection:

```bash
via ssh line-07                   # a shell (needs your key in the device's ssh keys)
via ssh line-07 journalctl -b     # a command
via forward line-07 8080 9000     # localhost:9000 on your machine -> device port 8080
```

Claude uses `exec` for the same thing, and the console has a terminal for the
device and for each of its containers.
