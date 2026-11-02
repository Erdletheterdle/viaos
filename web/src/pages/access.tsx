import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { ago, tool } from "../lib";
import { useIsAdmin, useMe } from "../main";
import { Button, Checkbox, Copy, Empty, Failure, Field, Headline, Input, Section } from "../ui";

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
      <Headline>Access</Headline>
      <div className="grid gap-x-5 lg:grid-cols-[minmax(0,1fr)_24rem] lg:items-start">
        <Section title="Tokens">
          <Failure error={tokens.error ?? revoke.error} />
          {tokens.data?.length === 0 && <Empty title="No named tokens yet.">Signing in with the bootstrap admin token works until you make one.</Empty>}
          {tokens.data?.map((t) => (
            <div key={t.name} className="grid grid-cols-[minmax(0,1fr)_5rem_auto] items-center gap-x-4 row text-sm sm:grid-cols-[minmax(0,1fr)_5rem_9rem_auto]">
              <span className="truncate font-medium">
                {t.name}
                {t.name === me.name && <span className="ml-2 font-mono text-xs font-normal text-ink-3">you</span>}
              </span>
              <span className="font-mono text-xs text-ink-2">{t.role}</span>
              <span className="font-mono text-xs text-ink-3 max-sm:hidden">used {ago(t.last_used)}</span>
              {admin && (
                <Button
                  tone="quiet"
                  size="sm"
                  className="hover:text-bad"
                  disabled={revoke.isPending || t.name === me.name}
                  title={t.name === me.name ? "This would sign you out with no way back in" : undefined}
                  onClick={() => revoke.mutate(t.name)}
                >
                  Revoke
                </Button>
              )}
            </div>
          ))}
        </Section>

        {admin && (
          <div>
            <Section title="New token">
              <form
                className="space-y-4 py-3"
                onSubmit={(e) => {
                  e.preventDefault();
                  create.mutate();
                }}
              >
                <Field label="For">
                  <Input required value={name} onChange={(e) => setName(e.target.value)} placeholder="ana, or ci" />
                </Field>
                <Checkbox checked={viewer} onChange={(e) => setViewer(e.target.checked)}>
                  Viewer: can look, can't change anything
                </Checkbox>
                <Failure error={create.error} />
                <Button tone="primary" type="submit" disabled={!name.trim() || create.isPending}>
                  Make token
                </Button>
              </form>
              {create.data && (
                <div className="mb-3 border-l-2 border-rule-strong pl-3">
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
