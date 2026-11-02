import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { tool, when } from "../lib";
import { Checkbox, Failure, Headline, Pip } from "../ui";

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
      <Headline aside={<Checkbox checked={reads} onChange={(e) => setReads(e.target.checked)}>Include reads</Checkbox>}>Audit</Headline>
      <Failure error={log.error} />
      <div className="border border-rule bg-sheet px-3 [&>*:last-child]:border-b-0">
        {log.data && entries.length === 0 && <p className="py-6 text-sm text-ink-2">Nothing has changed yet.</p>}
        {entries.map((e, i) => (
          <div key={i} className="grid grid-cols-[1rem_10.5rem_8rem_minmax(0,1fr)] items-center gap-x-3 row text-sm max-md:grid-cols-[1rem_minmax(0,1fr)]">
            <span className="self-center">
              <Pip state={e.ok ? "ok" : "bad"} />
            </span>
            <span className="font-mono text-xs text-ink-3 max-md:hidden">{when(e.at)}</span>
            <span className="truncate font-medium max-md:hidden">{e.actor || "–"}</span>
            <span className="min-w-0 truncate" title={detail(e.args)}>
              <span className="font-mono text-[0.82rem]">{e.tool}</span>
              <span className="ml-3 font-mono text-xs text-ink-3">{detail(e.args)}</span>
            </span>
          </div>
        ))}
      </div>
    </>
  );
}
