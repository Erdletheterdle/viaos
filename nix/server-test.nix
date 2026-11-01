# The control plane as the NixOS module runs it: behind Caddy, with tokens,
# roles, MCP, WebSockets and a large upload going through the proxy.
{ self, pkgs }:
pkgs.testers.runNixOSTest {
  name = "via-server";
  nodes.machine = {
    imports = [ self.nixosModules.via-server ];
    services.via-server = {
      enable = true;
      domain = "via.test";
    };
    # No ACME in the sandbox: Caddy's own CA instead.
    services.caddy.globalConfig = "local_certs";
    networking.hosts."127.0.0.1" = [ "via.test" ];
    environment.systemPackages = [ pkgs.curl ];
    virtualisation.diskSize = 2048;
  };
  testScript = ''
    import json

    def mcp(token, method, params):
        body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
        out = machine.succeed(
            f"curl -sf http://127.0.0.1:8471/mcp -H 'Authorization: Bearer {token}' "
            "-H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' "
            "-H 'MCP-Protocol-Version: 2025-06-18' "
            f"-d '{body}'"
        )
        return json.loads(out)

    def call(token, tool, args):
        return mcp(token, "tools/call", {"name": tool, "arguments": args})["result"]

    machine.wait_for_unit("via-server.service")
    machine.wait_until_succeeds("curl -sf http://127.0.0.1:8471/healthz")

    admin = machine.succeed("sudo -u via-server VIA_DATA_DIR=/var/lib/via-server via-server token create ana").strip()
    viewer = machine.succeed("sudo -u via-server VIA_DATA_DIR=/var/lib/via-server via-server token create bo --role viewer").strip()

    machine.fail("curl -sf http://127.0.0.1:8471/mcp -H 'Authorization: Bearer nope' -d '{}'")

    r = call(admin, "create_fleet", {"fleet": "line"})
    assert not r.get("isError"), r
    r = call(viewer, "list_fleets", {})
    assert r["structuredContent"]["fleets"][0]["name"] == "line", r
    r = call(viewer, "create_fleet", {"fleet": "other"})
    assert r.get("isError"), r

    # Uploads and tunnels are admin-only.
    machine.fail(f"curl -sf -X PUT http://127.0.0.1:8471/v1/base/x86_64 -H 'Authorization: Bearer {viewer}' -d x")

    # Through Caddy, over TLS: the API, a WebSocket upgrade (a device's
    # connection; refused for want of a credential, but by via-server, not
    # the proxy), and an upload bigger than any default body limit.
    machine.wait_for_unit("caddy.service")
    ca = "/var/lib/caddy/.local/share/caddy/pki/authorities/local/root.crt"
    https = f"curl -sf --cacert {ca} https://via.test"
    machine.wait_until_succeeds(f"{https}/healthz")
    code = machine.succeed(
        f"curl -s -o /dev/null -w '%{{http_code}}' --cacert {ca} --http1.1 https://via.test/v1/connect "
        "-H 'Connection: Upgrade' -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' "
        "-H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ=='"
    )
    assert code == "401", code
    machine.succeed("head -c 300M /dev/urandom > /tmp/big")
    machine.succeed(f"{https}/v1/os/x86_64/9/viaos_9.efi -X PUT -H 'Authorization: Bearer {admin}' -T /tmp/big")
    machine.succeed("cmp /tmp/big /var/lib/via-server/os/x86_64/9/viaos_9.efi || cmp /tmp/big $(find /var/lib/via-server -name viaos_9.efi | head -1)")

    machine.succeed("sudo -u via-server VIA_DATA_DIR=/var/lib/via-server via-server token revoke bo")
    machine.fail(f"curl -sf http://127.0.0.1:8471/mcp -H 'Authorization: Bearer {viewer}' -d '{{}}'")
  '';
}
