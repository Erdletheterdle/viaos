import { Link } from "react-router-dom";
import { createContext, useContext, useEffect, useId, useRef, useState, type ButtonHTMLAttributes, type InputHTMLAttributes, type ReactNode, type TextareaHTMLAttributes } from "react";

type Tone = "primary" | "plain" | "danger" | "quiet";

const tones: Record<Tone, string> = {
  primary: "bg-act text-act-ink border-act hover:opacity-85",
  plain: "border-rule-strong bg-sheet text-ink hover:bg-well",
  danger: "border-bad bg-sheet text-bad hover:bg-bad hover:text-paper",
  quiet: "border-transparent text-ink-2 hover:border-rule hover:text-ink",
};

const button = "inline-flex cursor-pointer items-center justify-center gap-2 border whitespace-nowrap transition-colors disabled:cursor-default disabled:opacity-40";
const sizes = {
  md: "h-9 px-3.5 text-sm font-medium max-sm:h-11",
  sm: "h-7 px-2 font-mono text-[0.68rem] font-medium tracking-[0.09em] uppercase max-sm:h-9",
};

export function buttonClass(tone: Tone = "plain", size: keyof typeof sizes = "md") {
  return `${button} ${sizes[size]} ${tones[tone]}`;
}

export function Button({ tone = "plain", size = "md", className = "", ...p }: { tone?: Tone; size?: keyof typeof sizes } & ButtonHTMLAttributes<HTMLButtonElement>) {
  return <button type="button" {...p} className={`${buttonClass(tone, size)} ${className}`} />;
}

// 16px on phones: iOS zooms the page into anything smaller when it's focused.
const field = "h-9 w-full border border-rule bg-well px-2.5 text-sm text-ink placeholder:text-ink-3 focus:border-rule-strong focus:outline-none disabled:opacity-50 max-sm:h-11 max-sm:text-base";

export function Input({ className = "", ...p }: InputHTMLAttributes<HTMLInputElement>) {
  return <input {...p} className={`${field} ${className}`} />;
}

export function Textarea({ className = "", ...p }: TextareaHTMLAttributes<HTMLTextAreaElement>) {
  return <textarea {...p} className={`${field} h-auto py-2 font-mono text-xs max-sm:h-auto ${className}`} />;
}

export function Checkbox({ children, className = "", ...p }: { children?: ReactNode } & Omit<InputHTMLAttributes<HTMLInputElement>, "type">) {
  return (
    <label className={`inline-flex cursor-pointer items-center gap-2 text-sm max-sm:min-h-11 ${className}`}>
      <span className="relative inline-flex size-[1.125rem] shrink-0">
        <input {...p} type="checkbox" className="absolute inset-0 m-0 size-full cursor-pointer opacity-0" />
        <span aria-hidden className="check inline-flex size-full items-center justify-center">
          <svg viewBox="0 0 12 12" className="size-3">
            <path d="M2 6.5 5 9.5 10 3" fill="none" stroke="currentColor" strokeWidth="2" />
          </svg>
        </span>
      </span>
      {children}
    </label>
  );
}

export type Option = { value: string; label: string; detail?: string; group?: string; badge?: string };

function shownText(o: Option) {
  return o.badge ? `${o.label} · ${o.badge}` : o.label;
}

/** Pick from a list the server supplied, by typing or arrowing. With
 *  `creatable`, whatever is typed is the value and the list only suggests. */
export function Combobox({
  options,
  value,
  onChange,
  placeholder,
  creatable,
  className = "",
  mono,
  required,
  disabled,
  "aria-label": ariaLabel,
}: {
  options: Option[];
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  creatable?: boolean;
  className?: string;
  mono?: boolean;
  required?: boolean;
  disabled?: boolean;
  "aria-label"?: string;
}) {
  const id = useId();
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [typed, setTyped] = useState(false);
  const [active, setActive] = useState(0);
  const list = useRef<HTMLUListElement>(null);
  const selected = options.find((o) => o.value === value);
  const terms = typed ? query.toLowerCase().split(/\s+/).filter(Boolean) : [];
  const matches = options.filter((o) => {
    const text = `${o.label} ${o.value} ${o.detail ?? ""} ${o.group ?? ""} ${o.badge ?? ""}`.toLowerCase();
    return terms.every((t) => text.includes(t));
  });

  useEffect(() => {
    list.current?.querySelector('[data-active="true"]')?.scrollIntoView({ block: "nearest" });
  }, [active, open]);

  const openList = () => {
    if (open || disabled) return;
    setOpen(true);
    setTyped(false);
    setQuery(creatable ? value : "");
    setActive(Math.max(0, options.findIndex((o) => o.value === value)));
  };
  const pick = (o: Option) => {
    onChange(o.value);
    setOpen(false);
    setTyped(false);
  };

  const text = creatable ? value : open ? query : selected ? shownText(selected) : "";
  return (
    <div className={`relative min-w-0 ${className}`}>
      <input
        role="combobox"
        aria-label={ariaLabel}
        aria-expanded={open}
        aria-controls={`${id}-list`}
        aria-autocomplete="list"
        aria-activedescendant={open && matches[active] ? `${id}-${active}` : undefined}
        autoComplete="off"
        spellCheck={false}
        required={required}
        disabled={disabled}
        value={text}
        placeholder={open && selected ? shownText(selected) : placeholder}
        className={`${field} pr-8 text-ellipsis ${mono ? "font-mono" : ""}`}
        onFocus={openList}
        onClick={openList}
        onBlur={() => {
          setOpen(false);
          setTyped(false);
        }}
        onChange={(e) => {
          setQuery(e.target.value);
          setTyped(true);
          setActive(0);
          setOpen(true);
          if (creatable) onChange(e.target.value);
        }}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            if (!open) return openList();
            const step = e.key === "ArrowDown" ? 1 : -1;
            setActive((a) => (matches.length ? (a + step + matches.length) % matches.length : 0));
          } else if (e.key === "Enter" && open && matches[active] && (!creatable || typed === false || matches[active].value.toLowerCase().startsWith(query.toLowerCase()))) {
            e.preventDefault();
            pick(matches[active]);
          } else if (e.key === "Enter" && !creatable) {
            e.preventDefault();
          } else if (e.key === "Escape" && open) {
            // Only the list: a dialog around this stays open.
            e.preventDefault();
            e.stopPropagation();
            setOpen(false);
          }
        }}
      />
      <svg aria-hidden viewBox="0 0 12 12" className="pointer-events-none absolute top-1/2 right-2.5 size-3 -translate-y-1/2 text-ink-3">
        <path d="M2 4.5 6 8.5 10 4.5" fill="none" stroke="currentColor" strokeWidth="1.5" />
      </svg>
      {open && (matches.length > 0 || !creatable) && (
        <ul ref={list} id={`${id}-list`} role="listbox" className="absolute z-30 mt-px max-h-64 w-full min-w-56 overflow-auto border border-rule-strong bg-sheet text-ink shadow-lg">
          {matches.length === 0 && <li className="px-2.5 py-2 text-sm text-ink-3">No match</li>}
          {matches.map((o, i) => (
            <li key={o.value} role="presentation">
              {o.group && o.group !== matches[i - 1]?.group && <p className="label border-b border-rule bg-well px-2.5 py-1">{o.group}</p>}
              <div
                id={`${id}-${i}`}
                role="option"
                aria-selected={o.value === value}
                data-active={i === active}
                className={`cursor-pointer px-2.5 py-1.5 text-sm max-sm:py-2.5 ${i === active ? "bg-well" : ""}`}
                onMouseEnter={() => setActive(i)}
                onMouseDown={(e) => e.preventDefault()}
                onClick={(e) => {
                  e.preventDefault();
                  e.stopPropagation();
                  pick(o);
                }}
              >
                <span className="flex items-baseline justify-between gap-3">
                  <span className={`min-w-0 truncate ${o.value === value ? "font-semibold" : ""} ${mono ? "font-mono" : ""}`}>{o.label}</span>
                  {o.badge && <Badge tone={o.badge === "latest" ? "latest" : undefined}>{o.badge}</Badge>}
                </span>
                {o.detail && <span className="block truncate text-xs text-ink-3">{o.detail}</span>}
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

const badgeTones = { warn: "border-warn text-warn", running: "border-ok text-ok", latest: "border-info text-info" };

export function Badge({ children, tone }: { children: ReactNode; tone?: keyof typeof badgeTones }) {
  return <span className={`inline-block shrink-0 border px-1 py-px font-mono text-[0.62rem] font-medium tracking-[0.08em] uppercase ${tone ? badgeTones[tone] : "border-rule-strong text-ink-2"}`}>{children}</span>;
}

/** A tag as written, with a `key=value` tag's key set back. */
export function Tag({ tag, children, quiet }: { tag: string; children?: ReactNode; quiet?: boolean }) {
  const at = tag.indexOf("=");
  return (
    <span className={`inline-flex h-5 max-w-full shrink-0 items-center border px-1 font-mono text-xs whitespace-nowrap ${quiet ? "border-rule text-ink-3" : "border-rule-strong"}`} title={quiet ? `${tag}, from its fleet` : tag}>
      {at > 0 ? (
        <>
          <span className="text-ink-3">{tag.slice(0, at)}=</span>
          {tag.slice(at + 1)}
        </>
      ) : (
        tag
      )}
      {children}
    </span>
  );
}

export function Field({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <div className="block">
      <span className="label mb-1.5 block">{label}</span>
      {children}
      {hint && <span className="mt-1 block text-xs text-ink-3">{hint}</span>}
    </div>
  );
}

const pips = { ok: "bg-ok", warn: "bg-warn", bad: "bg-bad", off: "bg-off" };

/** A square status lamp. */
export function Pip({ state }: { state: keyof typeof pips }) {
  return <span aria-hidden className={`inline-block size-2.5 shrink-0 ${pips[state]}`} />;
}

/** A number and what it counts, for page heads. */
export function Readout({ n, label, state }: { n: number; label: string; state?: keyof typeof pips }) {
  return (
    <span className="inline-flex items-center gap-1.5 font-mono text-xs">
      {state && <Pip state={state} />}
      <span className="font-semibold">{n}</span>
      <span className="text-ink-3">{label}</span>
    </span>
  );
}

/** The page's heading is where it sits: each level up is a link, and the
 *  last part is this page. */
export function Crumbs({ trail, current }: { trail?: [string, string][]; current: ReactNode }) {
  return (
    <h1 className="flex min-w-0 flex-1 flex-wrap items-baseline gap-x-1.5 text-sm">
      {trail?.map(([label, to]) => (
        <span key={to} className="flex items-baseline gap-x-2">
          <Link to={to} className="text-ink-2 underline-offset-4 hover:text-ink hover:underline">
            {label}
          </Link>
          <span aria-hidden className="text-ink-3">/</span>
        </span>
      ))}
      <span aria-current="page" className="min-w-0 font-semibold break-words">
        {current}
      </span>
    </h1>
  );
}

/** Opens the sidebar on a phone. */
export const Menu = createContext<() => void>(() => {});

/** The bar at the top of every page: where you are, and whatever acts on it.
 *  It's always the same height, so what's below stays put as you navigate. */
export function Headline({ children, aside, trail }: { children: ReactNode; aside?: ReactNode; trail?: [string, string][] }) {
  const openMenu = useContext(Menu);
  return (
    <header className="sticky top-0 z-20 -mx-4 mb-4 flex min-h-12 flex-wrap items-center gap-x-4 gap-y-1 border-b border-rule bg-sheet px-4 py-1 sm:-mx-5 sm:px-5">
      <Button tone="quiet" size="sm" className="-ml-2 px-2 lg:hidden" aria-label="Open navigation" onClick={openMenu}>
        <svg aria-hidden viewBox="0 0 16 16" className="size-4">
          <path d="M2 4h12M2 8h12M2 12h12" fill="none" stroke="currentColor" strokeWidth="1.5" />
        </svg>
      </Button>
      <Crumbs trail={trail} current={children} />
      {aside && <div className="ml-auto flex flex-wrap items-center gap-x-4 gap-y-1">{aside}</div>}
    </header>
  );
}

/** A panel with a title strip. Its rows bring their own rules; the last drops its own. */
export function Section({ title, aside, children, className = "" }: { title: ReactNode; aside?: ReactNode; children: ReactNode; className?: string }) {
  return (
    <section className={`mb-5 border border-rule bg-sheet ${className}`}>
      <div className="flex min-h-8 items-center justify-between gap-3 border-b border-rule bg-well px-3 py-1">
        <h2 className="label">{title}</h2>
        {aside}
      </div>
      <div className="px-3 [&>*:last-child]:border-b-0">{children}</div>
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

export function Notice({ children }: { children: ReactNode }) {
  return <div className="border-l-2 border-warn py-1 pl-3 text-sm">{children}</div>;
}

export function Empty({ title, children }: { title: string; children?: ReactNode }) {
  return (
    <div className="border-b border-rule py-6">
      <p className="text-sm font-semibold">{title}</p>
      {children && <div className="mt-1 max-w-xl text-sm text-ink-2">{children}</div>}
    </div>
  );
}

export function Modal({ title, onClose, children }: { title: string; onClose: () => void; children: ReactNode }) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    ref.current?.showModal();
  }, []);
  return (
    <dialog ref={ref} onClose={onClose} className="m-auto w-[min(34rem,calc(100vw-2rem))] border border-rule-strong bg-sheet p-0 text-ink">
      <h2 className="label border-b border-rule bg-well px-4 py-2.5">{title}</h2>
      <div className="p-4">{children}</div>
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
