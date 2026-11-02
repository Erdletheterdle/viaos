import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { signIn } from "../lib";
import { Mark } from "../main";
import { Button, Failure, Input } from "../ui";

export function Login() {
  const qc = useQueryClient();
  const [token, setToken] = useState("");
  const [error, setError] = useState<unknown>();
  const [busy, setBusy] = useState(false);
  return (
    <main className="grid min-h-dvh lg:grid-cols-[1.25fr_1fr]">
      <div className="flex flex-col justify-between border-b-[1.5px] border-rule-strong p-6 sm:p-10 lg:border-r-[1.5px] lg:border-b-0">
        <div className="flex items-center gap-2 font-display text-xl font-bold tracking-tight">
          <Mark className="size-5" />
          viaos
        </div>
        <p className="rise mt-16 max-w-3xl font-display text-[clamp(2.6rem,7vw,5.6rem)] leading-[0.98] font-semibold tracking-[-0.03em]">
          Every device, <span className="marked">wherever</span> it ended up.
        </p>
      </div>
      <form
        className="flex flex-col justify-center gap-4 p-6 sm:p-10"
        onSubmit={async (e) => {
          e.preventDefault();
          setBusy(true);
          setError(undefined);
          try {
            qc.setQueryData(["session"], await signIn(token));
          } catch (err) {
            setError(err);
          } finally {
            setBusy(false);
          }
        }}
      >
        <label className="block max-w-md">
          <span className="label mb-1.5 block">API token</span>
          <Input autoFocus type="password" autoComplete="current-password" value={token} onChange={(e) => setToken(e.target.value)} placeholder="via_…" />
        </label>
        <div className="max-w-md">
          <Failure error={error} />
        </div>
        <div>
          <Button tone="primary" type="submit" disabled={!token.trim() || busy}>
            {busy ? "Checking…" : "Sign in"}
          </Button>
        </div>
        <p className="max-w-md text-sm text-ink-2">
          Make one on the server with <code className="font-mono text-[0.82rem] text-ink">via-server token create &lt;you&gt;</code>
        </p>
      </form>
    </main>
  );
}
