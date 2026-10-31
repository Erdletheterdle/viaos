# Running a control plane

The control plane is one binary with one SQLite file. It needs a public
HTTPS address that devices can reach. Devices dial out to it, so they can
be behind NAT; the server cannot be.

## 1. Run it

On NixOS, with Caddy getting the certificate:

```nix
{
  inputs.viaos.url = "github:<you>/viaos";
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

## 4. Give it an OS

```bash
export VIA_SERVER=https://via.example.com VIA_TOKEN=<token>
export VIA_SIGNING_KEY=~/.config/via/signing.key
nix build .#images.x86_64 -o image && via base-image image/viaos_1.raw --arch x86_64
nix build .#images.aarch64 -o image-arm && via base-image image-arm/viaos_1.raw --arch aarch64
```

For each new OS version, bump `VERSION`, then run
`nix build .#updates.x86_64 -o update` and `via os-release update`.
`.github/workflows/release.yml` does both architectures on a `v<N>` tag.

## 5. Point Claude at it

```bash
claude mcp add --transport http via https://via.example.com/mcp \
  --header "Authorization: Bearer $VIA_TOKEN"
claude mcp add via-local -- sudo -E via mcp    # flashing, releases
```

## Reaching a device

Devices are behind NAT, so everything goes over their own connection:

```bash
via ssh line-07                   # a shell (needs your key in the device's ssh keys)
via ssh line-07 journalctl -b     # a command
via forward line-07 8080 9000     # localhost:9000 on your machine -> device port 8080
```

Claude uses `exec` for the same thing.
