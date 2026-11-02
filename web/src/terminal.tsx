import { FitAddon } from "@xterm/addon-fit";
import { Terminal as XTerm } from "@xterm/xterm";
import "@xterm/xterm/css/xterm.css";
import { useEffect, useRef, useState } from "react";
import { Button } from "./ui";

/** A shell on a device, or in one of its containers, over its one connection. */
export function Terminal({ device, container }: { device: string; container?: string }) {
  const host = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<"connecting" | "open" | "closed">("connecting");
  const [attempt, setAttempt] = useState(0);

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
        background: "#0f0e0c",
        foreground: "#e3e0d8",
        cursor: "#e6e94a",
        cursorAccent: "#0f0e0c",
        selectionBackground: "#e6e94a",
        selectionForeground: "#0f0e0c",
      },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current);
    fit.fit();

    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const query = container ? `?container=${encodeURIComponent(container)}` : "";
    const ws = new WebSocket(`${proto}//${location.host}/v1/devices/${encodeURIComponent(device)}/shell${query}`);
    ws.binaryType = "arraybuffer";
    const size = () => {
      if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify({ cols: term.cols, rows: term.rows }));
    };
    ws.onopen = () => {
      setState("open");
      size();
      term.focus();
    };
    ws.onmessage = (e) => term.write(new Uint8Array(e.data as ArrayBuffer));
    ws.onclose = () => setState("closed");
    const enc = new TextEncoder();
    const typed = term.onData((d) => {
      if (ws.readyState === WebSocket.OPEN) ws.send(enc.encode(d));
    });
    const resized = term.onResize(size);
    const seen = new ResizeObserver(() => fit.fit());
    seen.observe(host.current);
    return () => {
      seen.disconnect();
      typed.dispose();
      resized.dispose();
      ws.close();
      term.dispose();
    };
  }, [device, container, attempt]);

  return (
    <div className="tty relative h-full min-h-[28rem] p-3">
      <div ref={host} className="absolute inset-3" />
      {state === "closed" && (
        <div className="absolute inset-x-0 bottom-0 flex items-center justify-between gap-4 bg-black/80 px-4 py-2.5 text-sm">
          <span>Session ended.</span>
          <Button className="border-white/50 text-white hover:bg-white hover:text-black" onClick={() => setAttempt((a) => a + 1)}>
            Reconnect
          </Button>
        </div>
      )}
    </div>
  );
}
