import { FitAddon } from "@xterm/addon-fit";
import { Terminal as XTerm } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useEffect, useRef, useState } from "react";
import { Button } from "./ui";

/** A shell on a device, or in one of its containers, over its one connection. */
export function Terminal({ device, container, active = true }: { device: string; container?: string; active?: boolean }) {
  const host = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<"connecting" | "open" | "closed">("connecting");
  const [attempt, setAttempt] = useState(0);
  const session = useRef<XTerm | null>(null);

  useEffect(() => {
    if (!host.current) return;
    setState("connecting");
    const term = new XTerm({
      fontFamily: '"JetBrains Mono Variable", ui-monospace, monospace',
      fontSize: 13,
      lineHeight: 1.25,
      cursorBlink: true,
      scrollback: 5000,
      theme: {
        background: "#0e1012",
        foreground: "#dfe2e6",
        cursor: "#dfe2e6",
        cursorAccent: "#0e1012",
        selectionBackground: "#dfe2e6",
        selectionForeground: "#0e1012",
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    session.current = term;
    // A hidden tab has no size to fit to; it fits when it's shown.
    const refit = () => {
      if (host.current?.clientWidth) fit.fit();
    };
    refit();

    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const query = container ? `?container=${encodeURIComponent(container)}` : "";
    const ws = new WebSocket(`${proto}//${location.host}/v1/devices/${encodeURIComponent(device)}/shell${query}`);
    ws.binaryType = "arraybuffer";
    const hostVisible = () => !!host.current?.clientWidth;
    const size = () => {
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ cols: term.cols, rows: term.rows }));
    };
    ws.onopen = () => {
      setState("open");
      size();
      if (hostVisible()) term.focus();
    };
    ws.onmessage = (e) => term.write(new Uint8Array(e.data as ArrayBuffer));
    ws.onclose = () => setState("closed");
    const enc = new TextEncoder();
    const typed = term.onData((d) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(enc.encode(d));
    });
    const resized = term.onResize(size);
    const seen = new ResizeObserver(refit);
    seen.observe(host.current);
    return () => {
      seen.disconnect();
      typed.dispose();
      resized.dispose();
      ws.close();
      term.dispose();
    };
  }, [device, container, attempt]);

  useEffect(() => {
    if (active) session.current?.focus();
  }, [active]);

  return (
    <div className="tty relative h-full min-h-[20rem] p-2 lg:p-3" style={{ background: "#0e1012" }}>
      <div ref={host} className="absolute inset-2 lg:inset-3" />
      {state === "closed" && (
        <div className="absolute inset-x-0 bottom-0 flex items-center justify-between gap-4 border-t border-rule bg-sheet px-4 py-2 text-sm">
          <span>Session ended.</span>
          <Button onClick={() => setAttempt((a) => a + 1)}>
            Reconnect
          </Button>
        </div>
      )}
    </div>
  );
}
