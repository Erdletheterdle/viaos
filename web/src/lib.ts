// Everything the console knows, it asks through the same MCP tools Claude
// uses. Roles and the audit log therefore cover both, with nothing to keep in
// step.

export class SignedOut extends Error {}

const PROTOCOL = "2026-07-28";

export async function tool<T = any>(name: string, args: Record<string, unknown> = {}): Promise<T> {
  const res = await fetch("/mcp", {
    method: "POST",
    headers: {
      "Content-Type": "application/json",
      Accept: "application/json, text/event-stream",
      "MCP-Protocol-Version": PROTOCOL,
      "Mcp-Method": "tools/call",
      "Mcp-Name": name,
    },
    body: JSON.stringify({
      jsonrpc: "2.0",
      id: 1,
      method: "tools/call",
      params: {
        name,
        arguments: args,
        _meta: {
          "io.modelcontextprotocol/protocolVersion": PROTOCOL,
          "io.modelcontextprotocol/clientInfo": { name: "via-console", version: "0" },
          "io.modelcontextprotocol/clientCapabilities": {},
        },
      },
    }),
  });
  if (res.status === 401) throw new SignedOut("signed out");
  if (!res.ok) throw new Error((await res.text()) || `${name}: ${res.status}`);
  const body = await res.json();
  if (body.error) throw new Error(body.error.message);
  const r = body.result;
  const text = r.content?.[0]?.text ?? "";
  if (r.isError) throw new Error(text || `${name} failed`);
  if (r.structuredContent) return r.structuredContent;
  try {
    return JSON.parse(text);
  } catch {
    return text as T;
  }
}

export type Caller = { name: string; role: "admin" | "viewer" };

export async function whoami(): Promise<Caller | null> {
  const res = await fetch("/v1/session");
  return res.ok ? res.json() : null;
}

export async function signIn(token: string): Promise<Caller> {
  const res = await fetch("/v1/session", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ token }),
  });
  if (!res.ok) throw new Error(res.status === 401 ? "That token isn't one this server knows." : await res.text());
  return res.json();
}

export async function signOut() {
  await fetch("/v1/session", { method: "DELETE" });
}

export type Device = {
  id: string;
  name: string;
  fleet: string;
  arch: string | null;
  os_version: string | null;
  agent_version: string | null;
  enrolled_at: number;
  last_seen: number | null;
  online: boolean;
};

export type Fleet = { name: string; devices: number };

export type Release = {
  app: string;
  release: number;
  artifacts: { arch: string; sha256: string; size_mb: number }[];
};

export type OsVersion = { arch: string; version: string; files: string[]; size_mb: number };

export type Variable = {
  app: string;
  key: string;
  scope: "fleet" | "device";
  target: string;
  secret: boolean;
  value: string | null;
  updated_at: number;
  updated_by: string;
};

export type Health = {
  host: {
    hostname: string;
    os: string | null;
    kernel: string | null;
    uptime_secs: number | null;
    load: [number, number, number] | null;
    cpus: number;
    memory: { total_mb: number; available_mb: number; used_percent: number } | null;
    disks: { mount: string; total_mb: number; free_mb: number; used_percent: number }[];
    temperatures_c: { zone: string; celsius: number }[];
  };
  os: {
    version: string | null;
    secure_boot: boolean;
    update: { version: string; state: string; progress?: number; error?: string } | null;
  };
  engine: { version?: string; error?: string } | null;
  release: {
    app?: string;
    current: string | null;
    previous: string | null;
    deploying: string | null;
    converged: boolean;
    missing_env?: Record<string, string[]>;
    error?: string;
  };
  deploy: { app: string; release: number; state: string; error?: string; rolled_back_to?: string } | null;
  services: { service: string; container?: string; status: string; health: string }[] | { error: string };
};

export function ago(unix: number | null | undefined): string {
  if (!unix) return "never";
  const s = Math.max(0, Date.now() / 1000 - unix);
  if (s < 10) return "just now";
  if (s < 60) return `${Math.floor(s)}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

export function duration(secs: number | null | undefined): string {
  if (secs == null) return "–";
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m`;
  if (secs < 86400) return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m`;
  return `${Math.floor(secs / 86400)}d ${Math.floor((secs % 86400) / 3600)}h`;
}

export function gb(mb: number): string {
  return mb < 1024 ? `${mb} MB` : `${(mb / 1024).toFixed(1)} GB`;
}

export function when(unix: number): string {
  return new Date(unix * 1000).toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}
