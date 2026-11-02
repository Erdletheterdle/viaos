// The text form of an app's variables on one fleet or device: an env file,
// with `[service]` sections and a `secret` prefix. Comment lines are ignored.

import type { SchemaVariable, Variable } from "./lib";

export const MASK = "••••••••";

export type Entry = { key: string; service: string | null; value: string; secret: boolean };
export type Change = { set: Entry[]; delete: { key: string; service: string | null }[] };

const id = (key: string, service: string | null) => `${service ?? ""}\0${key}`;

function line(e: { key: string; value: string | null; secret: boolean }) {
  return e.secret ? `secret ${e.key}=${MASK}` : `${e.key}=${e.value ?? ""}`;
}

export function format(own: Variable[], inherited: Variable[], declared: SchemaVariable[]): string {
  const out: string[] = [];
  for (const d of declared) out.push(`# ${d.key}=${d.default ?? ""}${d.required && d.default == null ? "   (required)" : ""}${d.description ? `   ${d.description}` : ""}`);
  for (const v of inherited) out.push(`# ${v.service ? `[${v.service}] ` : ""}${line(v)}   (from fleet ${v.target})`);
  if (out.length) out.push("");
  for (const v of own.filter((v) => !v.service).sort((a, b) => a.key.localeCompare(b.key))) out.push(line(v));
  const services = [...new Set(own.flatMap((v) => (v.service ? [v.service] : [])))].sort();
  for (const s of services) {
    out.push("", `[${s}]`);
    for (const v of own.filter((v) => v.service === s).sort((a, b) => a.key.localeCompare(b.key))) out.push(line(v));
  }
  return out.join("\n").trim() + "\n";
}

export function parse(text: string): { entries: Entry[]; errors: string[] } {
  const entries: Entry[] = [];
  const errors: string[] = [];
  const seen = new Set<string>();
  let service: string | null = null;
  text.split("\n").forEach((raw, i) => {
    const l = raw.trim();
    if (!l || l.startsWith("#")) return;
    const section = l.match(/^\[([^\]]*)\]$/);
    if (section) {
      service = section[1].trim() || null;
      return;
    }
    const m = raw.trimStart().match(/^(secret\s+)?([A-Za-z_][A-Za-z0-9_]*)=(.*)$/);
    if (!m) {
      errors.push(`Line ${i + 1}: expected NAME=value`);
      return;
    }
    const key = m[2];
    if (seen.has(id(key, service))) errors.push(`Line ${i + 1}: ${key} is set twice${service ? ` in [${service}]` : ""}`);
    seen.add(id(key, service));
    entries.push({ key, service, value: m[3], secret: !!m[1] });
  });
  return { entries, errors };
}

/** What has to change to make the stored values match the text. */
export function diff(own: Variable[], entries: Entry[]): { change: Change; errors: string[] } {
  const was = new Map(own.map((v) => [id(v.key, v.service ?? null), v]));
  const now = new Set(entries.map((e) => id(e.key, e.service)));
  const set: Entry[] = [];
  const errors: string[] = [];
  for (const e of entries) {
    const old = was.get(id(e.key, e.service));
    if (e.value === MASK) {
      // A mask stands for the stored secret. It can't become anything else.
      if (!old?.secret) errors.push(`${e.key}: give it a value`);
      else if (!e.secret) errors.push(`${e.key}: its value can't be shown, so give it a new one to stop it being secret`);
      continue;
    }
    if (old && old.secret === e.secret && !e.secret && old.value === e.value) continue;
    set.push(e);
  }
  const del = own.filter((v) => !now.has(id(v.key, v.service ?? null))).map((v) => ({ key: v.key, service: v.service ?? null }));
  return { change: { set, delete: del }, errors };
}
