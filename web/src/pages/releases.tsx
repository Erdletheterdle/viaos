import { useMutation, useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { gb, tool, type OsVersion, type Release } from "../lib";
import { useIsAdmin } from "../main";
import { Button, Empty, Failure, Field, Headline, Modal, Section, Select } from "../ui";
import { useDevices, useFleets } from "./fleet";

export type Run = { title: string; tool: string; args: Record<string, unknown>; warning?: string };

type Outcome = { device: string; skipped?: string; would_run?: boolean; started?: boolean; error?: string };

/** Show exactly which devices an action touches, then do it. */
export function RunDialog({ run, onClose }: { run: Run; onClose: () => void }) {
  const preview = useQuery({
    queryKey: ["dry-run", run.tool, run.args],
    queryFn: () => tool<{ devices: Outcome[] }>(run.tool, { ...run.args, dry_run: true }),
    gcTime: 0,
  });
  const go = useMutation({ mutationFn: () => tool<{ devices: Outcome[] }>(run.tool, run.args) });
  const rows = go.data?.devices ?? preview.data?.devices ?? [];
  const runnable = preview.data?.devices.some((d) => d.would_run);
  return (
    <Modal title={run.title} onClose={onClose}>
      {run.warning && !go.data && <p className="mb-3 text-sm text-ink-2">{run.warning}</p>}
      <Failure error={preview.error ?? go.error} />
      <div className="max-h-72 overflow-auto">
        {rows.map((d) => (
          <div key={d.device} className="flex items-baseline justify-between gap-4 border-b border-rule py-2 text-sm">
            <span className="font-medium">{d.device}</span>
            <span className={`text-right text-xs ${d.error ? "text-bad" : d.skipped ? "text-ink-3" : ""}`}>
              {d.error ?? (d.skipped ? `skipped: ${d.skipped}` : d.started ? "started" : "will run")}
            </span>
          </div>
        ))}
        {preview.data && rows.length === 0 && <p className="py-2 text-sm text-ink-2">No devices there.</p>}
      </div>
      <div className="mt-5 flex justify-end gap-2">
        <form method="dialog">
          <Button type="submit">{go.data ? "Close" : "Cancel"}</Button>
        </form>
        {!go.data && (
          <Button tone="primary" disabled={!runnable || go.isPending} onClick={() => go.mutate()}>
            {go.isPending ? "Starting…" : "Go"}
          </Button>
        )}
      </div>
    </Modal>
  );
}

function Target({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const fleets = useFleets();
  const devices = useDevices();
  useEffect(() => {
    if (!value && fleets.data?.[0]) onChange(`fleet:${fleets.data[0].name}`);
  }, [value, fleets.data, onChange]);
  return (
    <Select aria-label="Target" value={value} onChange={(e) => onChange(e.target.value)}>
      <optgroup label="Whole fleet">
        {fleets.data?.map((f) => (
          <option key={f.name} value={`fleet:${f.name}`}>
            {f.name} · {f.devices} {f.devices === 1 ? "device" : "devices"}
          </option>
        ))}
      </optgroup>
      <optgroup label="One device">
        {devices.data?.map((d) => (
          <option key={d.id} value={`device:${d.name}`}>
            {d.name}
          </option>
        ))}
      </optgroup>
    </Select>
  );
}

function targetArgs(target: string) {
  const [kind, name] = [target.slice(0, target.indexOf(":")), target.slice(target.indexOf(":") + 1)];
  return { [kind]: name };
}

export function Releases() {
  const admin = useIsAdmin();
  const releases = useQuery({
    queryKey: ["releases"],
    queryFn: () => tool<{ releases: Release[] }>("list_releases").then((r) => r.releases),
  });
  const versions = useQuery({
    queryKey: ["os-versions"],
    queryFn: () => tool<{ versions: OsVersion[] }>("list_os_versions").then((r) => r.versions),
  });
  const [target, setTarget] = useState("");
  const [run, setRun] = useState<Run | null>(null);

  const apps = Object.entries(
    (releases.data ?? []).reduce<Record<string, Release[]>>((all, r) => {
      (all[r.app] ??= []).push(r);
      return all;
    }, {}),
  );
  const osByVersion = Object.entries(
    (versions.data ?? []).reduce<Record<string, OsVersion[]>>((all, v) => {
      (all[v.version] ??= []).push(v);
      return all;
    }, {}),
  ).sort((a, b) => Number(b[0]) - Number(a[0]));
  const label = target.slice(target.indexOf(":") + 1);

  return (
    <>
      <Headline
        aside={
          admin ? (
            <div className="w-64">
              <Field label="Send to">
                <Target value={target} onChange={setTarget} />
              </Field>
            </div>
          ) : undefined
        }
      >
        What the fleet can run, and <span className="marked">where it goes next</span>.
      </Headline>

      <Section title="Apps">
        <Failure error={releases.error} />
        {releases.data?.length === 0 && <Empty title="No releases uploaded.">Build and upload one from a compose file with <code className="font-mono text-xs">via release -r 1</code>.</Empty>}
        {apps.map(([app, rs]) => (
          <div key={app} className="grid gap-x-8 border-b border-rule py-3 sm:grid-cols-[12rem_minmax(0,1fr)]">
            <p className="font-display text-lg font-semibold tracking-tight">{app}</p>
            <div>
              {rs.map((r) => (
                <div key={r.release} className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 py-1">
                  <span className="font-mono text-sm">release {r.release}</span>
                  <span className="flex flex-1 flex-wrap gap-x-3 font-mono text-xs text-ink-3">
                    {r.artifacts.map((a) => (
                      <span key={a.arch}>
                        {a.arch} {gb(a.size_mb)}
                      </span>
                    ))}
                  </span>
                  {admin && (
                    <Button
                      disabled={!target}
                      onClick={() =>
                        setRun({
                          title: `Deploy ${app} ${r.release} to ${label}`,
                          tool: "deploy",
                          args: { app, release: r.release, ...targetArgs(target) },
                          warning: "Containers are replaced. A device that fails the health gate goes back to its previous release by itself.",
                        })
                      }
                    >
                      Deploy
                    </Button>
                  )}
                </div>
              ))}
            </div>
          </div>
        ))}
      </Section>

      <Section title="Operating system">
        <Failure error={versions.error} />
        {versions.data?.length === 0 && <Empty title="No OS versions uploaded.">Publish one with <code className="font-mono text-xs">via os-release</code>.</Empty>}
        {osByVersion.map(([version, vs]) => (
          <div key={version} className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1 border-b border-rule py-2.5">
            <span className="w-48 font-display text-lg font-semibold tracking-tight">Version {version}</span>
            <span className="flex flex-1 flex-wrap gap-x-3 font-mono text-xs text-ink-3">
              {vs.map((v) => (
                <span key={v.arch}>
                  {v.arch} {gb(v.size_mb)}
                </span>
              ))}
            </span>
            {admin && (
              <Button
                disabled={!target}
                onClick={() =>
                  setRun({
                    title: `Update ${label} to OS ${version}`,
                    tool: "update_os",
                    args: { version, ...targetArgs(target) },
                    warning: "Each device reboots, stopping its containers for about a minute. A version that fails to boot rolls back by itself.",
                  })
                }
              >
                Update
              </Button>
            )}
          </div>
        ))}
      </Section>
      {run && <RunDialog run={run} onClose={() => setRun(null)} />}
    </>
  );
}
