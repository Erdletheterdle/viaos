import { expect, test } from "bun:test";
import { diff, format, MASK, parse } from "../src/envtext";
import type { Variable } from "../src/lib";

const v = (key: string, value: string | null, secret = false, service: string | null = null): Variable => ({ app: "hello", key, service, scope: "fleet", target: "lab", secret, value, updated_at: 0, updated_by: "t" });
const stored = [v("GREETING", "bonjour"), v("API_KEY", null, true), v("LEVEL", "debug", false, "web")];

test("unchanged text changes nothing", () => {
  const { entries, errors } = parse(format(stored, [], []));
  expect(errors).toEqual([]);
  expect(diff(stored, entries)).toEqual({ change: { set: [], delete: [] }, errors: [] });
});

test("a secret's value is never in the text", () => {
  const text = format(stored, [], []);
  expect(text).toContain(`secret API_KEY=${MASK}`);
});

test("edits, additions and removals, section by section", () => {
  const { entries } = parse(`GREETING=hola\nNEW=1=2\n\n[web]\nOTHER=x\n[worker]\nsecret TOKEN=abc\n`);
  const { change, errors } = diff(stored, entries);
  expect(errors).toEqual([]);
  expect(change.set).toEqual([
    { key: "GREETING", service: null, value: "hola", secret: false },
    { key: "NEW", service: null, value: "1=2", secret: false },
    { key: "OTHER", service: "web", value: "x", secret: false },
    { key: "TOKEN", service: "worker", value: "abc", secret: true },
  ]);
  expect(change.delete).toEqual([{ key: "API_KEY", service: null }, { key: "LEVEL", service: "web" }]);
});

test("comments are ignored, so inherited values aren't copied here", () => {
  const text = format([], [v("GREETING", "bonjour")], [{ key: "X", services: [], description: "d", default: null, secret: false, required: true }]);
  expect(text).toContain("(from fleet lab)");
  expect(text).toContain("(required)");
  expect(parse(text).entries).toEqual([]);
});

test("mistakes are reported, not guessed at", () => {
  expect(parse("nonsense\n").errors[0]).toContain("Line 1");
  expect(parse("A=1\nA=2\n").errors[0]).toContain("twice");
  expect(diff(stored, parse(`A=${MASK}\n`).entries).errors[0]).toContain("give it a value");
  expect(diff(stored, parse(`API_KEY=${MASK}\n`).entries).errors[0]).toContain("can't be shown");
});

import { hasTag, tagChoices } from "../src/lib";

test("a bare key matches any value of it, and a tag only itself", () => {
  const tags = ["site=Dallas North", "eu"];
  expect(hasTag(tags, "site")).toBe(true);
  expect(hasTag(tags, "site=Dallas North")).toBe(true);
  expect(hasTag(tags, "site=Austin")).toBe(false);
  expect(hasTag(tags, "eu")).toBe(true);
  expect(hasTag(tags, "si")).toBe(false);
});

test("a filter offers each key that has values, and each tag", () => {
  expect(tagChoices(["site=a", "site=b", "site=a", "eu"]).map((o) => o.value)).toEqual(["eu", "site", "site=a", "site=b"]);
});
