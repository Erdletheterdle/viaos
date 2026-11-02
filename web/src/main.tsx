import { QueryCache, QueryClient, QueryClientProvider, useQuery, useQueryClient } from "@tanstack/react-query";
import { createContext, StrictMode, useContext, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Link, NavLink, Route, Routes } from "react-router-dom";
import "./index.css";
import { SignedOut, signOut, whoami, type Caller } from "./lib";
import { Access } from "./pages/access";
import { Audit } from "./pages/audit";
import { DevicePage } from "./pages/device";
import { Enroll } from "./pages/enroll";
import { FleetPage } from "./pages/fleet";
import { Login } from "./pages/login";
import { Releases } from "./pages/releases";
import { Variables } from "./pages/variables";

const client = new QueryClient({
  queryCache: new QueryCache({
    onError: (e) => {
      if (e instanceof SignedOut) client.setQueryData(["session"], null);
    },
  }),
  defaultOptions: {
    queries: {
      retry: (n, e) => !(e instanceof SignedOut) && n < 1,
      refetchOnWindowFocus: true,
    },
  },
});

const Me = createContext<Caller>({ name: "", role: "viewer" });
export const useMe = () => useContext(Me);
export const useIsAdmin = () => useMe().role === "admin";

type Theme = "light" | "dark" | "auto";

function applyTheme(theme: Theme) {
  const dark = theme === "dark" || (theme === "auto" && matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.dataset.theme = dark ? "dark" : "light";
}

function storedTheme(): Theme {
  try {
    return (localStorage.getItem("via-theme") as Theme) || "auto";
  } catch {
    return "auto";
  }
}
applyTheme(storedTheme());

function ThemeSwitch() {
  const [theme, setTheme] = useState<Theme>(storedTheme);
  useEffect(() => {
    applyTheme(theme);
    try {
      localStorage.setItem("via-theme", theme);
    } catch {}
    const q = matchMedia("(prefers-color-scheme: dark)");
    const on = () => applyTheme(theme);
    q.addEventListener("change", on);
    return () => q.removeEventListener("change", on);
  }, [theme]);
  const next: Record<Theme, Theme> = { auto: "light", light: "dark", dark: "auto" };
  return (
    <button className="label cursor-pointer hover:text-ink" onClick={() => setTheme(next[theme])} title="Change theme">
      {theme}
    </button>
  );
}

function Mark({ className = "" }: { className?: string }) {
  return (
    <svg viewBox="0 0 16 16" aria-hidden className={className}>
      <path fill="currentColor" d="M3 3h4v10H3v-2h2V5H3V3Zm6 0h4v2h-2v6h2v2H9V3Z" />
    </svg>
  );
}
export { Mark };

const nav = [
  ["/", "Fleet"],
  ["/enroll", "Enroll"],
  ["/releases", "Releases"],
  ["/variables", "Variables"],
  ["/audit", "Audit"],
  ["/access", "Access"],
] as const;

function Masthead({ me }: { me: Caller }) {
  const qc = useQueryClient();
  return (
    <header className="sticky top-0 z-20 border-b-[1.5px] border-rule-strong bg-paper">
      <div className="mx-auto flex max-w-[92rem] flex-wrap items-center gap-x-8 gap-y-1 px-4 py-2.5 sm:px-8">
        <Link to="/" className="flex items-center gap-2 font-display text-xl font-bold tracking-tight">
          <Mark className="size-5" />
          viaos
        </Link>
        <nav className="order-3 -mx-1 flex w-full gap-5 overflow-x-auto px-1 text-sm sm:order-none sm:w-auto">
          {nav.map(([to, label]) => (
            <NavLink
              key={to}
              to={to}
              end={to === "/"}
              className={({ isActive }) => (isActive ? "font-semibold" : "text-ink-2 hover:text-ink")}
            >
              {({ isActive }) => <span className={isActive ? "marked" : ""}>{label}</span>}
            </NavLink>
          ))}
        </nav>
        <div className="ml-auto flex items-center gap-4">
          <span className="label">
            {me.name}
            {me.role === "viewer" && " · viewer"}
          </span>
          <ThemeSwitch />
          <button
            className="label cursor-pointer hover:text-ink"
            onClick={async () => {
              await signOut();
              qc.setQueryData(["session"], null);
              qc.clear();
            }}
          >
            Sign out
          </button>
        </div>
      </div>
    </header>
  );
}

function App() {
  const session = useQuery({ queryKey: ["session"], queryFn: whoami, staleTime: 60_000 });
  if (session.isPending) return null;
  if (!session.data) return <Login />;
  return (
    <Me.Provider value={session.data}>
      <BrowserRouter>
        <Masthead me={session.data} />
        <main className="mx-auto max-w-[92rem] px-4 pt-9 pb-20 sm:px-8">
          <Routes>
            <Route path="/" element={<FleetPage />} />
            <Route path="/devices/:name" element={<DevicePage />} />
            <Route path="/enroll" element={<Enroll />} />
            <Route path="/releases" element={<Releases />} />
            <Route path="/variables" element={<Variables />} />
            <Route path="/audit" element={<Audit />} />
            <Route path="/access" element={<Access />} />
            <Route path="*" element={<FleetPage />} />
          </Routes>
        </main>
      </BrowserRouter>
    </Me.Provider>
  );
}

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <QueryClientProvider client={client}>
      <App />
    </QueryClientProvider>
  </StrictMode>,
);
