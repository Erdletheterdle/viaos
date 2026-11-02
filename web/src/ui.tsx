import { useEffect, useRef, useState, type ButtonHTMLAttributes, type InputHTMLAttributes, type ReactNode, type SelectHTMLAttributes } from "react";

type Tone = "primary" | "plain" | "danger";

export function Button({ tone = "plain", className = "", ...p }: { tone?: Tone } & ButtonHTMLAttributes<HTMLButtonElement>) {
  const tones: Record<Tone, string> = {
    primary: "bg-act text-act-ink border-act hover:opacity-85",
    plain: "border-rule-strong text-ink hover:bg-ink hover:text-paper",
    danger: "border-bad text-bad hover:bg-bad hover:text-paper",
  };
  return (
    <button
      type="button"
      {...p}
      className={`inline-flex h-9 cursor-pointer items-center justify-center gap-2 border px-3.5 text-sm font-medium whitespace-nowrap transition-colors disabled:cursor-default disabled:opacity-40 disabled:hover:bg-transparent disabled:hover:text-inherit ${tones[tone]} ${className}`}
    />
  );
}

const field =
  "h-9 w-full border border-rule bg-sheet px-2.5 text-sm text-ink placeholder:text-ink-3 focus:border-rule-strong focus:outline-none";

export function Input({ className = "", ...p }: InputHTMLAttributes<HTMLInputElement>) {
  return <input {...p} className={`${field} ${className}`} />;
}

export function Select({ className = "", ...p }: SelectHTMLAttributes<HTMLSelectElement>) {
  return <select {...p} className={`${field} ${className}`} />;
}

export function Field({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <label className="block">
      <span className="label mb-1.5 block">{label}</span>
      {children}
      {hint && <span className="mt-1 block text-xs text-ink-3">{hint}</span>}
    </label>
  );
}

const pips = { ok: "bg-ok", warn: "bg-warn", bad: "bg-bad", off: "bg-off" };

/** A square status lamp. */
export function Pip({ state, live }: { state: keyof typeof pips; live?: boolean }) {
  return <span aria-hidden className={`inline-block size-2.5 shrink-0 ${pips[state]} ${live ? "live" : ""}`} />;
}

/** Page opening: a large plain sentence rather than a row of stat cards. */
export function Headline({ children, aside }: { children: ReactNode; aside?: ReactNode }) {
  return (
    <header className="rise mb-9 flex flex-wrap items-end justify-between gap-x-8 gap-y-4">
      <h1 className="max-w-4xl font-display text-[clamp(2rem,5vw,3.4rem)] leading-[1.02] font-semibold tracking-[-0.025em] text-balance">
        {children}
      </h1>
      {aside}
    </header>
  );
}

export function Section({ title, aside, children }: { title: string; aside?: ReactNode; children: ReactNode }) {
  return (
    <section className="mb-10">
      <div className="mb-0 flex items-baseline justify-between border-b-[1.5px] border-rule-strong pb-1.5">
        <h2 className="font-display text-lg font-semibold tracking-tight">{title}</h2>
        {aside}
      </div>
      {children}
    </section>
  );
}

export function Failure({ error }: { error: unknown }) {
  if (!error) return null;
  return (
    <p role="alert" className="border-l-2 border-bad py-1 pl-3 text-sm text-bad">
      {error instanceof Error ? error.message : String(error)}
    </p>
  );
}

export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="border-b border-rule py-10">
      <p className="font-display text-xl font-medium tracking-tight">{title}</p>
      {children && <div className="mt-1.5 max-w-xl text-sm text-ink-2">{children}</div>}
    </div>
  );
}

export function Modal({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    ref.current?.showModal();
  }, []);
  return (
    <dialog
      ref={ref}
      onClose={onClose}
      className="m-auto w-[min(34rem,calc(100vw-2rem))] border-[1.5px] border-rule-strong bg-sheet p-6 text-ink"
    >
      <h2 className="mb-4 font-display text-2xl font-semibold tracking-tight">{title}</h2>
      {children}
    </dialog>
  );
}

export function Copy({ text, label = "Copy" }: { text: string; label?: string }) {
  const [done, setDone] = useState(false);
  return (
    <Button
      onClick={() => {
        navigator.clipboard?.writeText(text);
        setDone(true);
        setTimeout(() => setDone(false), 1400);
      }}
    >
      {done ? "Copied" : label}
    </Button>
  );
}
