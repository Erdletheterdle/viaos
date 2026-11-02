import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// `bun run dev` proxies to a local via-server. The server's cookie check
// compares Origin with Host, so the proxy keeps the browser's Host.
const server = process.env.VIA_SERVER ?? "http://localhost:8471";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: { chunkSizeWarningLimit: 800 },
  server: {
    proxy: {
      "/mcp": { target: server },
      "/v1": { target: server, ws: true },
    },
  },
});
