import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import { fileURLToPath, URL } from "node:url";

// Tauri loads ui/dist (release) or this dev server (debug). Fixed port: tauri.conf.json devUrl.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) } },
  // Inline (empty) PostCSS config: stops Vite from walking up into the Selene repo and loading its
  // PostCSS/Tailwind v3 setup (this project must stay isolated from Selene's build). Tailwind 4 runs
  // through @tailwindcss/vite instead.
  css: { postcss: {} },
  clearScreen: false,
  server: { port: 5173, strictPort: true, host: "127.0.0.1" },
  build: { target: "es2022", outDir: "dist", emptyOutDir: true, sourcemap: false },
});
