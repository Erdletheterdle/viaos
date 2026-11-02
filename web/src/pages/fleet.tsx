import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { Link, useNavigate, useSearchParams } from "react-router-dom";
import { ago, tool, type Device, type Fleet } from "../lib";
import { useIsAdmin } from "../main";
import { Button, Empty, Failure, Headline, Input, Pip } from "../ui";

export function useDevices() {
  return useQuery({
    queryKey: ["devices"],
    queryFn: () => tool<{ devices: Device[] }>("list_devices").then((r) => r.devices),
    refetchInterval: 5000,
  });
}

export function useFleets() {
  return useQuery({
    queryKey: ["fleets"],
    queryFn: () => tool<{ fleets: Fleet[] }>("list_fleets").then((r) => r.fleets),
  });
}

function sentence(devices: Device[]) {
  const on = devices.filter((d) => d.online).length;
  if (devices.length === 0) return <>Nothing enrolled yet.</>;
  if (on === devices.length)
    return (
      <>
        {devices.length === 1 ? "The device is" : `All ${devices.length} devices are`} <span className="marked">online</span>.
      </>
    );
  return (
    <>
      {on} of {devices.length} devices online, <span className="marked">{devices.length - on} not answering</span>.
    </>
  );
}

function NewFleet() {
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const create = useMutation({
    mutationFn: () => tool("create_fleet", { fleet: name.trim() }),
    onSuccess: () => {
      setName("");
      qc.invalidateQueries({ queryKey: ["fleets"] });
    },
  });
  return (
    <form
      className="flex items-start gap-2"
      onSubmit={(e) => {
        e.preventDefault();
        create.mutate();
      }}
    >
      <div>
        <Input className="w-44" aria-label="New fleet name" placeholder="New fleet" value={name} onChange={(e) => setName(e.target.value)} />
        <Failure error={create.error} />
      </div>
      <Button type="submit" disabled={!name.trim() || create.isPending}>
        Add
      </Button>
    </form>
  );
}

export function FleetPage() {
  const admin = useIsAdmin();
  const nav = useNavigate();
  const devices = useDevices();
  const fleets = useFleets();
  const [params, setParams] = useSearchParams();
  const [filter, setFilter] = useState("");
  const fleet = params.get("fleet");

  const all = devices.data ?? [];
  const shown = all
    .filter((d) => !fleet || d.fleet === fleet)
    .filter((d) => !filter || `${d.name} ${d.fleet} ${d.arch} ${d.os_version}`.toLowerCase().includes(filter.toLowerCase()))
    .sort((a, b) => Number(a.online) - Number(b.online) || a.name.localeCompare(b.name));

  return (
    <>
      <Headline aside={admin ? <NewFleet /> : undefined}>{devices.isPending ? " " : sentence(all)}</Headline>
      <Failure error={devices.error} />

      <div className="flex flex-wrap items-center justify-between gap-x-6 gap-y-2 border-b-[1.5px] border-rule-strong pb-2">
        <div className="flex flex-wrap gap-x-4 gap-y-1 text-sm">
          <button className={`cursor-pointer ${!fleet ? "font-semibold" : "text-ink-2 hover:text-ink"}`} onClick={() => setParams({})}>
            {!fleet ? <span className="marked">Every fleet</span> : "Every fleet"}
          </button>
          {(fleets.data ?? []).map((f) => (
            <button
              key={f.name}
              className={`cursor-pointer ${fleet === f.name ? "font-semibold" : "text-ink-2 hover:text-ink"}`}
              onClick={() => setParams({ fleet: f.name })}
            >
              {fleet === f.name ? <span className="marked">{f.name}</span> : f.name}
              <span className="ml-1 font-mono text-xs text-ink-3">{f.devices}</span>
            </button>
          ))}
        </div>
        <input
          aria-label="Filter devices"
          placeholder="Filter…"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          className="w-40 border-b border-rule bg-transparent py-0.5 text-sm placeholder:text-ink-3 focus:border-rule-strong focus:outline-none"
        />
      </div>

      {!devices.isPending && all.length === 0 && (
        <Empty title="Make an image, boot a device, and it shows up here.">
          <Link className="underline underline-offset-3" to="/enroll">
            Enroll the first device
          </Link>
        </Empty>
      )}

      {shown.length > 0 && (
        <div className="overflow-x-auto">
          <table className="w-full min-w-[38rem] border-collapse text-sm">
            <thead>
              <tr className="border-b border-rule text-left">
                {["Device", "Fleet", "Arch", "OS", "Agent", "Last seen"].map((h, i) => (
                  <th key={h} className={`label py-2 pr-4 font-medium ${i === 5 ? "pr-0 text-right" : ""}`}>
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {shown.map((d) => (
                <tr
                  key={d.id}
                  onClick={() => nav(`/devices/${encodeURIComponent(d.name)}`)}
                  className="group cursor-pointer border-b border-rule transition-colors hover:bg-sheet"
                >
                  <td className="py-3 pr-4">
                    <div className="flex items-center gap-3">
                      <Pip state={d.online ? "ok" : "bad"} />
                      <Link
                        to={`/devices/${encodeURIComponent(d.name)}`}
                        onClick={(e) => e.stopPropagation()}
                        className="font-display text-[1.05rem] font-semibold tracking-tight group-hover:underline group-hover:underline-offset-3"
                      >
                        {d.name}
                      </Link>
                    </div>
                  </td>
                  <td className="py-3 pr-4 text-ink-2">{d.fleet}</td>
                  <td className="py-3 pr-4 font-mono text-xs text-ink-2">{d.arch ?? "–"}</td>
                  <td className="py-3 pr-4 font-mono text-xs">{d.os_version ?? "–"}</td>
                  <td className="py-3 pr-4 font-mono text-xs text-ink-2">{d.agent_version ?? "–"}</td>
                  <td className={`py-3 text-right font-mono text-xs ${d.online ? "text-ink-3" : "text-bad"}`}>{d.online ? "connected" : ago(d.last_seen)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}
