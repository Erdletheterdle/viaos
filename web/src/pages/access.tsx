import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ago, tool } from "../lib";
import { useIsAdmin, useMe } from "../main";
import { Button, Copy, Empty, Failure, Field, Headline, Input, Section } from "../ui";

type Token = { name: string; role: string; created_at: number; last_used: number | null };

export function Access() {
  const admin = useIsAdmin();
  const me = useMe();
  const qc = useQueryClient();
  const [name, setName] = useState("");
  const [viewer, setViewer] = useState(false);
  const tokens = useQuery({
    queryKey: ["tokens"],
    queryFn: () => tool<{ tokens: Token[] }>("list_tokens").then((r) => r.tokens),
  });
  const create = useMutation({
    mutationFn: () => tool<{ name: string; token: string }>("create_token", { name: name.trim(), viewer }),
    onSuccess: () => {
      setName("");
      qc.invalidateQueries({ queryKey: ["tokens"] });
    },
  });
  const revoke = useMutation({
    mutationFn: (n: string) => tool("revoke_token", { name: n }),
    onSuccess: () => qc.invalidateQueries({ queryKey: ["tokens"] }),
  });
  const server = location.origin;
  return (
    <>
      <Headline>Each person and machine gets <span className="marked">its own token</span>.</Headline>
      <div className="grid gap-x-12 gap-y-8 lg:grid-cols-[minmax(0,1fr)_24rem]">
        <Section title="Tokens">
          <Failure error={tokens.error ?? revoke.error} />
          {tokens.data?.length === 0 && <Empty title="No named tokens yet.">Signing in with the bootstrap admin token works until you make one.</Empty>}
          {tokens.data?.map((t) => (
            <div key={t.name} className="grid grid-cols-[minmax(0,1fr)_5rem_auto] items-baseline gap-x-4 border-b border-rule py-2.5 text-sm sm:grid-cols-[minmax(0,1fr)_5rem_9rem_auto]">
              <span className="truncate font-medium">
                {t.name}
                {t.name === me.name && <span className="ml-2 font-mono text-xs font-normal text-ink-3">you</span>}
              </span>
              <span className="font-mono text-xs text-ink-2">{t.role}</span>
              <span className="font-mono text-xs text-ink-3 max-sm:hidden">used {ago(t.last_used)}</span>
              {admin && (
                <button
                  className="label cursor-pointer hover:text-bad disabled:opacity-40"
                  disabled={revoke.isPending || t.name === me.name}
                  title={t.name === me.name ? "This would sign you out with no way back in" : undefined}
                  onClick={() => revoke.mutate(t.name)}
                >
                  revoke
                </button>
              )}
            </div>
          ))}
        </Section>

        {admin && (
          <div>
            <Section title="New token">
              <form
                className="space-y-4 pt-4"
                onSubmit={(e) => {
                  e.preventDefault();
                  create.mutate();
                }}
              >
                <Field label="For">
                  <Input required value={name} onChange={(e) => setName(e.target.value)} placeholder="ana, or ci" />
                </Field>
                <label className="flex cursor-pointer items-center gap-2 text-sm">
                  <input type="checkbox" checked={viewer} onChange={(e) => setViewer(e.target.checked)} className="size-4 accent-[var(--act)]" />
                  Viewer: can look, can't change anything
                </label>
                <Failure error={create.error} />
                <Button tone="primary" type="submit" disabled={!name.trim() || create.isPending}>
                  Make token
                </Button>
              </form>
              {create.data && (
                <div className="mt-5 border-l-2 border-rule-strong pl-3">
                  <p className="text-sm font-medium">{create.data.name}'s token, shown this once:</p>
                  <p className="my-2 font-mono text-xs break-all">{create.data.token}</p>
                  <div className="flex flex-wrap gap-2">
                    <Copy text={create.data.token} label="Copy token" />
                    <Copy text={`claude mcp add --transport http via ${server}/mcp --header "Authorization: Bearer ${create.data.token}"`} label="Copy for Claude" />
                  </div>
                </div>
              )}
            </Section>
          </div>
        )}
      </div>
    </>
  );
}
