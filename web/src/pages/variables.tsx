import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ago, tool, type Variable } from "../lib";
import { useIsAdmin } from "../main";
import { Button, Empty, Failure, Field, Headline, Input, Section, Select } from "../ui";
import { useDevices, useFleets } from "./fleet";

function useVariables() {
  return useQuery({
    queryKey: ["variables"],
    queryFn: () => tool<{ variables: Variable[] }>("list_variables").then((r) => r.variables),
  });
}

type Outcome = { devices: { device: string; pending?: string; error?: string }[] };

function summary(r: Outcome) {
  const failed = r.devices.filter((d) => d.error);
  if (failed.length) return `Saved, but ${failed.map((d) => `${d.device}: ${d.error}`).join("; ")}`;
  const pending = r.devices.filter((d) => d.pending).length;
  const sent = r.devices.length - pending;
  return `Saved. Sent to ${sent} ${sent === 1 ? "device" : "devices"}${pending ? `, ${pending} offline will get it when they reconnect` : ""}.`;
}

function AddVariable({ preset }: { preset?: { app?: string; scope?: string } }) {
  const qc = useQueryClient();
  const fleets = useFleets();
  const devices = useDevices();
  const [app, setApp] = useState(preset?.app ?? "");
  const [key, setKey] = useState("");
  const [value, setValue] = useState("");
  const [secret, setSecret] = useState(true);
  const [picked, setPicked] = useState("");
  const scope = picked || preset?.scope || (fleets.data?.[0] ? `fleet:${fleets.data[0].name}` : "");
  const save = useMutation({
    mutationFn: () => {
      const i = scope.indexOf(":");
      return tool<Outcome>("set_variable", { app: app.trim(), key: key.trim(), value, secret, [scope.slice(0, i)]: scope.slice(i + 1) });
    },
    onSuccess: () => {
      setKey("");
      setValue("");
      qc.invalidateQueries({ queryKey: ["variables"] });
    },
  });
  return (
    <form
      autoComplete="off"
      className="space-y-4"
      onSubmit={(e) => {
        e.preventDefault();
        save.mutate();
      }}
    >
      <div className="grid gap-4 sm:grid-cols-2">
        <Field label="App">
          <Input required value={app} onChange={(e) => setApp(e.target.value)} placeholder="hello" />
        </Field>
        <Field label="Applies to">
          <Select value={scope} onChange={(e) => setPicked(e.target.value)}>
            <optgroup label="Fleet">
              {fleets.data?.map((f) => (
                <option key={f.name} value={`fleet:${f.name}`}>
                  {f.name}
                </option>
              ))}
            </optgroup>
            <optgroup label="Device (beats its fleet)">
              {devices.data?.map((d) => (
                <option key={d.id} value={`device:${d.name}`}>
                  {d.name}
                </option>
              ))}
            </optgroup>
          </Select>
        </Field>
      </div>
      <Field label="Name" hint="Services receive it only if their compose file lists it as a bare “- NAME”.">
        <Input required className="font-mono" value={key} onChange={(e) => setKey(e.target.value.toUpperCase())} placeholder="API_KEY" />
      </Field>
      <Field label="Value">
        <Input required type={secret ? "password" : "text"} autoComplete="new-password" className="font-mono" value={value} onChange={(e) => setValue(e.target.value)} />
      </Field>
      <label className="flex cursor-pointer items-center gap-2 text-sm">
        <input type="checkbox" checked={secret} onChange={(e) => setSecret(e.target.checked)} className="size-4 accent-[var(--act)]" />
        Secret: encrypted here, and never shown again
      </label>
      <Failure error={save.error} />
      {save.data && <p className="text-sm text-ink-2">{summary(save.data)}</p>}
      <Button tone="primary" type="submit" disabled={save.isPending || !scope}>
        {save.isPending ? "Saving…" : "Save"}
      </Button>
    </form>
  );
}

function VariableRow({ v, showApp, admin }: { v: Variable; showApp?: boolean; admin: boolean }) {
  const qc = useQueryClient();
  const remove = useMutation({
    mutationFn: () => tool("delete_variable", { app: v.app, key: v.key, [v.scope]: v.target }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["variables"] }),
  });
  return (
    <div className="grid grid-cols-[minmax(0,1fr)_auto] items-baseline gap-x-4 border-b border-rule py-2 text-sm sm:grid-cols-[minmax(0,14rem)_minmax(0,1fr)_9rem_auto]">
      <span className="truncate font-mono text-[0.82rem] font-medium">
        {showApp && <span className="font-normal text-ink-3">{v.app} / </span>}
        {v.key}
      </span>
      <span className="truncate font-mono text-xs text-ink-2 max-sm:col-span-2 max-sm:row-start-2">
        {v.secret ? <span className="text-ink-3">•••••••• secret</span> : v.value}
      </span>
      <span className="truncate text-xs text-ink-3 max-sm:hidden" title={`${v.updated_by}, ${ago(v.updated_at)}`}>
        {v.scope === "device" ? "device" : "fleet"} {v.target}
      </span>
      {admin && (
        <button className="label cursor-pointer text-right hover:text-bad" disabled={remove.isPending} onClick={() => remove.mutate()}>
          {remove.isPending ? "…" : "delete"}
        </button>
      )}
      {remove.error ? (
        <div className="col-span-full">
          <Failure error={remove.error} />
        </div>
      ) : null}
    </div>
  );
}

export function Variables() {
  const admin = useIsAdmin();
  const vars = useVariables();
  const apps = Object.entries(
    (vars.data ?? []).reduce<Record<string, Variable[]>>((all, v) => {
      (all[v.app] ??= []).push(v);
      return all;
    }, {}),
  );
  return (
    <>
      <Headline>Settings and secrets stay <span className="marked">out of the release</span>.</Headline>
      <div className="grid gap-x-12 gap-y-8 lg:grid-cols-[minmax(0,1fr)_24rem]">
        <div className="min-w-0">
          <Failure error={vars.error} />
          {vars.data?.length === 0 && (
            <Section title="Nothing set">
              <Empty title="Declare it in compose, set the value here.">
                <pre className="mt-2 font-mono text-xs leading-relaxed text-ink">{"services:\n  web:\n    environment:\n      - API_KEY"}</pre>
              </Empty>
            </Section>
          )}
          {apps.map(([app, vs]) => (
            <Section key={app} title={app}>
              {vs.map((v) => (
                <VariableRow key={`${v.scope}/${v.target}/${v.key}`} v={v} admin={admin} />
              ))}
            </Section>
          ))}
        </div>
        {admin && (
          <div>
            <Section title="Set a value">
              <div className="pt-4">
                <AddVariable />
              </div>
            </Section>
          </div>
        )}
      </div>
    </>
  );
}

/** What applies on one device: its fleet's values and its own. */
export function DeviceVariables({ device, fleet, app }: { device: string; fleet?: string; app?: string }) {
  const admin = useIsAdmin();
  const vars = useVariables();
  const mine = (vars.data ?? []).filter((v) => (v.scope === "device" && v.target === device) || (v.scope === "fleet" && v.target === fleet));
  const overridden = (v: Variable) => v.scope === "fleet" && mine.some((o) => o.scope === "device" && o.app === v.app && o.key === v.key);
  return (
    <div className="grid gap-x-10 gap-y-6 pt-1 xl:grid-cols-[minmax(0,1fr)_20rem]">
      <div className="min-w-0">
        <Failure error={vars.error} />
        {vars.data && mine.length === 0 && <p className="py-6 text-sm text-ink-2">Nothing applies to this device yet.</p>}
        {mine.filter((v) => !overridden(v)).map((v) => (
          <VariableRow key={`${v.scope}/${v.target}/${v.app}/${v.key}`} v={v} showApp admin={admin} />
        ))}
      </div>
      {admin && (
        <div className="pt-3">
          <AddVariable preset={{ app, scope: `device:${device}` }} />
        </div>
      )}
    </div>
  );
}
