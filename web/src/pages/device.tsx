import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef, useState } from "react";
import { Link, useParams, useSearchParams } from "react-router-dom";
import { ago, duration, gb, tool, type Health } from "../lib";
import { useIsAdmin } from "../main";
import { Terminal } from "../terminal";
import { Button, Failure, Pip, Select } from "../ui";
import { useDevices } from "./fleet";
import { DeviceVariables } from "./variables";
import { RunDialog, type Run } from "./releases";

function Gauge({ label, percent, detail }: { label: string; percent: number; detail: string }) {
  const hot = percent >= 90 ? "bg-bad" : percent >= 75 ? "bg-warn" : "bg-ink";
  return (
    <div>
      <div className="flex items-baseline justify-between gap-3">
        <span className="font-mono text-xs">{label}</span>
        <span className="font-mono text-xs text-ink-3">{detail}</span>
      </div>
      <div className="mt-1 h-1.5 bg-rule">
        <div className={`h-full ${hot}`} style={{ width: `${Math.min(100, percent)}%` }} />
      </div>
    </div>
  );
}

function Row({ k, v }: { k: string; v: React.ReactNode }) {
  return (
    <div className="flex items-baseline justify-between gap-4 border-b border-rule py-1.5 text-sm">
      <span className="label">{k}</span>
      <span className="text-right font-mono text-xs">{v}</span>
    </div>
  );
}

function serviceState(s: { status: string; health: string }): "ok" | "warn" | "bad" | "off" {
  const status = s.status.toLowerCase();
  const health = s.health.toLowerCase();
  if (!status.includes("running")) return status.includes("exit") || status.includes("dead") ? "bad" : "off";
  if (health.includes("unhealthy")) return "bad";
  if (health.includes("starting")) return "warn";
  return "ok";
}

const LINE_COLOURS = ["#e6e94a", "#7fd6a4", "#7fc3e6", "#e6a97f", "#c6a7e6", "#e67f9b"];

function Logs({ device, services }: { device: string; services: string[] }) {
  const [service, setService] = useState("");
  const [follow, setFollow] = useState(true);
  const [filter, setFilter] = useState("");
  const q = useQuery({
    queryKey: ["logs", device, service],
    queryFn: () =>
      tool<{ lines: { service: string; at?: string; line: string }[]; note?: string }>("logs", {
        device,
        tail: 500,
        ...(service ? { service } : {}),
      }),
    refetchInterval: follow ? 3000 : false,
  });
  const box = useRef<HTMLDivElement>(null);
  const lines = (q.data?.lines ?? []).filter((l) => !filter || l.line.toLowerCase().includes(filter.toLowerCase()));
  useEffect(() => {
    if (follow && box.current) box.current.scrollTop = box.current.scrollHeight;
  }, [follow, q.data, filter]);
  return (
    <div className="tty flex h-full min-h-[28rem] flex-col">
      <div className="flex flex-wrap items-center gap-3 border-b border-white/15 px-3 py-2 text-xs">
        <select aria-label="Service" value={service} onChange={(e) => setService(e.target.value)} className="border border-white/25 bg-transparent px-1.5 py-1 font-mono [&>option]:text-black">
          <option value="">every service</option>
          {services.map((s) => (
            <option key={s}>{s}</option>
          ))}
        </select>
        <input aria-label="Filter lines" placeholder="filter lines" value={filter} onChange={(e) => setFilter(e.target.value)} className="w-40 border border-white/25 bg-transparent px-1.5 py-1 font-mono placeholder:text-white/40" />
        <label className="ml-auto flex cursor-pointer items-center gap-1.5 font-mono">
          <input type="checkbox" checked={follow} onChange={(e) => setFollow(e.target.checked)} className="accent-[#e6e94a]" />
          follow
        </label>
      </div>
      <div ref={box} className="min-h-0 flex-1 overflow-auto p-3 font-mono text-[0.76rem] leading-[1.55]">
        {q.error ? <p className="text-[#f08a6b]">{(q.error as Error).message}</p> : null}
        {lines.length === 0 && !q.isPending && !q.error && <p style={{ color: "var(--tty-dim)" }}>{q.data?.note ?? "No output."}</p>}
        {lines.map((l, i) => (
          <div key={i} className="whitespace-pre-wrap break-all">
            {services.length > 1 && !service && (
              <span className="mr-2 select-none" style={{ color: LINE_COLOURS[Math.max(0, services.indexOf(l.service)) % LINE_COLOURS.length] }}>
                {l.service}
              </span>
            )}
            {l.line}
          </div>
        ))}
      </div>
    </div>
  );
}

function Journal({ device }: { device: string }) {
  const q = useQuery({
    queryKey: ["journal", device],
    queryFn: () => tool<{ entries: { at: string; event: string; release?: string | number; detail?: string }[] }>("journal", { device, limit: 100 }),
    refetchInterval: 8000,
  });
  return (
    <div>
      <Failure error={q.error} />
      {q.data?.entries.length === 0 && <p className="py-6 text-sm text-ink-2">Nothing deployed here yet.</p>}
      {q.data?.entries.map((e, i) => (
        <div key={i} className="grid grid-cols-[9.5rem_1fr] gap-x-4 border-b border-rule py-2 text-sm max-sm:grid-cols-1">
          <span className="font-mono text-xs text-ink-3">{e.at.replace("T", " ").slice(0, 19)}</span>
          <span>
            <span className="font-medium">{e.event.replaceAll("_", " ")}</span>
            {e.release != null && <span className="ml-2 font-mono text-xs text-ink-2">release {e.release}</span>}
            {e.detail && <span className="mt-0.5 block font-mono text-xs break-words text-ink-3">{e.detail}</span>}
          </span>
        </div>
      ))}
    </div>
  );
}

function OsUpdate({ device, arch, current, disabled }: { device: string; arch?: string | null; current?: string | null; disabled: boolean }) {
  const q = useQuery({
    queryKey: ["os-versions"],
    queryFn: () => tool<{ versions: { arch: string; version: string; size_mb: number }[] }>("list_os_versions").then((r) => r.versions),
  });
  const [run, setRun] = useState<Run | null>(null);
  const [version, setVersion] = useState("");
  const options = (q.data ?? []).filter((v) => v.arch === arch && v.version !== current);
  if (options.length === 0) return <p className="mt-2 text-xs text-ink-3">No other OS version published for {arch ?? "this device"}.</p>;
  const chosen = version || options[options.length - 1].version;
  return (
    <div className="mt-2 flex gap-2">
      <Select aria-label="OS version" value={chosen} onChange={(e) => setVersion(e.target.value)}>
        {options.map((v) => (
          <option key={v.version} value={v.version}>
            version {v.version} · {gb(v.size_mb)}
          </option>
        ))}
      </Select>
      <Button
        disabled={disabled}
        onClick={() =>
          setRun({ title: `Update ${device} to OS ${chosen}`, tool: "update_os", args: { version: chosen, device }, warning: "The device reboots. Its containers stop for about a minute." })
        }
      >
        Update
      </Button>
      {run && <RunDialog run={run} onClose={() => setRun(null)} />}
    </div>
  );
}

type Tab = "logs" | "terminal" | "journal" | "variables";

export function DevicePage() {
  const admin = useIsAdmin();
  const { name = "" } = useParams();
  const devices = useDevices();
  const device = devices.data?.find((d) => d.name === name);
  const online = !!device?.online;
  const [params, setParams] = useSearchParams();
  const tab = (params.get("tab") as Tab) || "logs";
  const container = params.get("container") ?? undefined;
  const open = (t: Tab, extra: Record<string, string> = {}) => setParams({ tab: t, ...extra }, { replace: true });

  const health = useQuery({
    queryKey: ["health", name],
    queryFn: () => tool<Health>("device_health", { device: name }),
    refetchInterval: 5000,
    enabled: online,
  });
  const h = online ? health.data : undefined;
  const services = Array.isArray(h?.services) ? h.services : [];
  const missing = Object.entries(h?.release.missing_env ?? {});
  const tabs: [Tab, string][] = [["logs", "Logs"], ...(admin ? ([["terminal", "Terminal"]] as [Tab, string][]) : []), ["journal", "Journal"], ["variables", "Variables"]];

  if (devices.isSuccess && !device) {
    return (
      <p className="py-10 text-sm">
        No device named {name}.{" "}
        <Link className="underline" to="/">
          Back to the fleet
        </Link>
      </p>
    );
  }

  return (
    <div className="grid gap-x-10 gap-y-8 lg:grid-cols-[21rem_minmax(0,1fr)]">
      <aside className="rise min-w-0">
        <Link to={device ? `/?fleet=${encodeURIComponent(device.fleet)}` : "/"} className="label hover:text-ink">
          ← {device?.fleet ?? "fleet"}
        </Link>
        <h1 className="mt-1 font-display text-[2.6rem] leading-none font-semibold tracking-[-0.025em] break-words">{name}</h1>
        <p className="mt-3 mb-6 flex items-center gap-2 text-sm">
          <Pip state={online ? "ok" : "bad"} live={online} />
          {online ? `Connected · up ${duration(h?.host.uptime_secs)}` : `Not answering · last seen ${ago(device?.last_seen)}`}
        </p>

        <Failure error={online ? health.error : null} />

        {missing.length > 0 && (
          <div className="mb-6 border-l-2 border-warn py-1 pl-3 text-sm">
            <p className="font-medium">Waiting on variables</p>
            {missing.map(([svc, keys]) => (
              <p key={svc} className="mt-0.5 font-mono text-xs text-ink-2">
                {svc}: {keys.join(", ")}
              </p>
            ))}
            <button className="mt-1 cursor-pointer text-xs underline underline-offset-2" onClick={() => open("variables")}>
              Set them
            </button>
          </div>
        )}

        <div className="mb-6">
          <p className="label mb-1 border-b-[1.5px] border-rule-strong pb-1.5">System</p>
          <Row k="OS" v={device?.os_version ?? "–"} />
          <Row k="Arch" v={device?.arch ?? "–"} />
          <Row k="Kernel" v={h?.host.kernel ?? "–"} />
          <Row k="Secure Boot" v={h ? (h.os.secure_boot ? "enforcing" : "off") : "–"} />
          <Row k="Docker" v={h?.engine?.version ?? "–"} />
          <Row k="Load" v={h?.host.load ? `${h.host.load[0].toFixed(2)} / ${h.host.cpus} cpu` : "–"} />
          {h?.os.update && (
            <p className={`mt-2 text-xs ${h.os.update.error ? "text-bad" : "text-ink-2"}`}>
              OS {h.os.update.version}: {h.os.update.error ?? `${h.os.update.state}${h.os.update.progress != null ? ` ${h.os.update.progress}%` : ""}`}
            </p>
          )}
          {admin && <OsUpdate device={name} arch={device?.arch} current={device?.os_version} disabled={!online} />}
        </div>

        {h && (
          <div className="mb-6 space-y-3">
            <p className="label border-b-[1.5px] border-rule-strong pb-1.5">Capacity</p>
            {h.host.memory && <Gauge label="memory" percent={h.host.memory.used_percent} detail={`${gb(h.host.memory.total_mb - h.host.memory.available_mb)} of ${gb(h.host.memory.total_mb)}`} />}
            {h.host.disks.map((d) => (
              <Gauge key={d.mount} label={d.mount} percent={d.used_percent} detail={`${gb(d.free_mb)} free`} />
            ))}
            {h.host.temperatures_c.slice(0, 3).map((t) => (
              <Row key={t.zone} k={t.zone} v={`${t.celsius.toFixed(0)}°C`} />
            ))}
          </div>
        )}

        {h && (
          <div>
            <p className="label border-b-[1.5px] border-rule-strong pb-1.5">
              {h.release.app ?? "App"}
              {h.release.current ? ` · release ${h.release.current}` : ""}
            </p>
            {services.length === 0 && <p className="py-2 text-sm text-ink-2">No release running.</p>}
            {services.map((s) => (
              <div key={s.service} className="flex items-center gap-2.5 border-b border-rule py-2 text-sm">
                <Pip state={serviceState(s)} />
                <span className="min-w-0 flex-1 truncate font-medium">{s.service}</span>
                <span className="font-mono text-xs text-ink-3">{s.health.toLowerCase() === "none" ? s.status.toLowerCase() : s.health.toLowerCase()}</span>
                {admin && s.container && (
                  <button className="label cursor-pointer hover:text-ink" title={`Shell in ${s.container}`} onClick={() => open("terminal", { container: s.container! })}>
                    shell
                  </button>
                )}
              </div>
            ))}
            {h.deploy && !["running"].includes(h.deploy.state) && (
              <p className={`mt-2 text-xs ${h.deploy.error ? "text-bad" : "text-ink-2"}`}>
                {h.deploy.app} {h.deploy.release}: {h.deploy.error ?? h.deploy.state}
              </p>
            )}
          </div>
        )}
      </aside>

      <section className="flex min-w-0 flex-col lg:h-[calc(100dvh-8.5rem)]">
        <div className="mb-0 flex items-center gap-5 border-b-[1.5px] border-rule-strong pb-2 text-sm">
          {tabs.map(([t, label]) => (
            <button key={t} className={`cursor-pointer ${tab === t ? "font-semibold" : "text-ink-2 hover:text-ink"}`} onClick={() => open(t)}>
              {tab === t ? <span className="marked">{label}</span> : label}
            </button>
          ))}
          {tab === "terminal" && container && (
            <button className="label ml-auto cursor-pointer hover:text-ink" onClick={() => open("terminal")}>
              {container} · back to the host
            </button>
          )}
        </div>
        <div className={`min-h-0 flex-1 ${tab === "journal" || tab === "variables" ? "overflow-auto" : ""}`}>
          {tab === "logs" &&
            (online ? (
              <Logs device={name} services={services.map((s) => s.service)} />
            ) : (
              <p className="py-6 text-sm text-ink-2">Logs live on the device. It has to be connected to read them.</p>
            ))}
          {tab === "terminal" &&
            (online ? (
              <Terminal device={name} container={container} />
            ) : (
              <p className="py-6 text-sm text-ink-2">A shell needs the device to be connected.</p>
            ))}
          {tab === "journal" &&
            (online ? (
              <Journal device={name} />
            ) : (
              <p className="py-6 text-sm text-ink-2">The journal lives on the device. It has to be connected to read it.</p>
            ))}
          {tab === "variables" && <DeviceVariables device={name} fleet={device?.fleet} app={h?.release.app} />}
        </div>
      </section>
    </div>
  );
}
