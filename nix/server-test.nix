# The control plane as the NixOS module runs it: tokens, roles, MCP.
{ self, pkgs }:
pkgs.testers.runNixOSTest {
  name = "via-server";
  nodes.machine = {
    imports = [ self.nixosModules.via-server ];
    services.via-server = {
      enable = true;
      publicUrl = "http://machine:8471";
    };
    environment.systemPackages = [ pkgs.curl ];
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

    machine.succeed("sudo -u via-server VIA_DATA_DIR=/var/lib/via-server via-server token revoke bo")
    machine.fail(f"curl -sf http://127.0.0.1:8471/mcp -H 'Authorization: Bearer {viewer}' -d '{{}}'")
  '';
}
