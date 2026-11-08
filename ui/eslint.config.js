// Lint rules. The "no polling" rules are product rules (P1.5: progress is pushed, never polled):
// status and progress arrive as Tauri events (`events.stateChanged`), so timers that re-ask the
// backend have no place in this UI. tests/no-polling.test.mjs proves the rules fire.
import js from "@eslint/js";
import tseslint from "typescript-eslint";
import reactHooks from "eslint-plugin-react-hooks";

export const noPollingRules = {
  "no-restricted-globals": [
    "error",
    { name: "setInterval", message: "No polling: subscribe to pushed events (events.stateChanged) instead." },
    { name: "setTimeout", message: "No timers: state is pushed by the app; use events or user actions." },
  ],
  "no-restricted-properties": [
    "error",
    { object: "window", property: "setInterval", message: "No polling: use pushed events." },
    { object: "window", property: "setTimeout", message: "No timers: use pushed events." },
    { object: "globalThis", property: "setInterval", message: "No polling: use pushed events." },
    { object: "globalThis", property: "setTimeout", message: "No timers: use pushed events." },
  ],
  "no-restricted-syntax": [
    "error",
    {
      selector: "Property[key.name=/^(refetchInterval|refetchIntervalInBackground)$/]",
      message: "No polling: TanStack Query data for status comes from pushed events (setQueryData).",
    },
  ],
};

export default tseslint.config(
  { ignores: ["dist/**", "node_modules/**", "src/bindings.ts", "tests/fixtures/**"] },
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ["src/**/*.{ts,tsx}"],
    plugins: { "react-hooks": reactHooks },
    rules: {
      ...reactHooks.configs.recommended.rules,
      ...noPollingRules,
    },
  },
  {
    // Node-side tooling (tests, dev scripts): not UI code, so the no-polling rules don't apply.
    files: ["tests/**/*.mjs", "scripts/**/*.mjs", "*.js"],
    languageOptions: {
      globals: {
        URL: "readonly", process: "readonly", console: "readonly", fetch: "readonly",
        WebSocket: "readonly", setTimeout: "readonly", Buffer: "readonly", structuredClone: "readonly",
      },
    },
  },
);
