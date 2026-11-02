import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { tool, when } from "../lib";
import { Failure, Headline, Pip } from "../ui";

type Entry = { at: number; actor: string | null; tool: string; args: Record<string, unknown>; ok: boolean };

/** Reads happen constantly and say little; changes are what you come here for. */
const READS = /^(list_|device_health$|logs$|journal$|sign_in$)/;

function detail(args: Record<string, unknown>) {
  return Object.entries(args ?? {})
    .filter(([, v]) => v !== null && v !== false && v !== undefined)
    .map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v)}`)
    .join("  ");
}

export function Audit() {
  const [reads, setReads] = useState(false);
  const log = useQuery({
    queryKey: ["audit"],
    queryFn: () => tool<{ entries: Entry[] }>("audit_log", { limit: 1000 }).then((r) => r.entries),
    refetchInterval: 8000,
  });
  const entries = (log.data ?? []).filter((e) => reads || !READS.test(e.tool));
  return (
    <>
      <Headline aside={
        <label className="flex cursor-pointer items-center gap-2 text-sm">
          <input type="checkbox" checked={reads} onChange={(e) => setReads(e.target.checked)} className="size-4 accent-[var(--act)]" />
          Include reads
        </label>
      }>
        Who did what, <span className="marked">and whether it worked</span>.
      </Headline>
      <Failure error={log.error} />
      <div className="border-t-[1.5px] border-rule-strong">
        {log.data && entries.length === 0 && <p className="py-6 text-sm text-ink-2">Nothing has changed yet.</p>}
        {entries.map((e, i) => (
          <div key={i} className="grid grid-cols-[1rem_10.5rem_8rem_minmax(0,1fr)] items-baseline gap-x-3 border-b border-rule py-2 text-sm max-md:grid-cols-[1rem_minmax(0,1fr)]">
            <span className="self-center">
              <Pip state={e.ok ? "ok" : "bad"} />
            </span>
            <span className="font-mono text-xs text-ink-3 max-md:hidden">{when(e.at)}</span>
            <span className="truncate font-medium max-md:hidden">{e.actor || "–"}</span>
            <span className="min-w-0">
              <span className="font-mono text-[0.82rem]">{e.tool}</span>
              <span className="ml-3 font-mono text-xs break-words text-ink-3">{detail(e.args)}</span>
            </span>
          </div>
        ))}
      </div>
    </>
  );
}
