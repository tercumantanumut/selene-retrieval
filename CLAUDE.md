# Selene Retrieval: agent instructions

Scope: this repository. This project is a standalone Rust MCP server plus a Windows-first Tauri desktop app.

## Status
The project is in beta; milestones, open items and risks are in `docs/04-roadmap.md` → "Milestone status". Amendments in `docs/00-review-and-decisions.md` are binding. Work only inside this repository; Selene is a separate repository that is read, never changed. No git commits unless the user asks.

## Product Principles (mandatory, read before any planning or implementation)
The full text is in `docs/product-principles.md`. In short:

- **Average-user simplicity is a core principle.** A non-technical user installs, sets up and uses the product from first launch without documentation.
  - One installer (releases are not code-signed yet; signing is planned). No terminal, no config edits, no extra runtimes.
  - Sensible defaults. Local embeddings work with no API key. Optional features (iteration pass, remote providers, MCP `add_folder`) are off by default.
  - A first-run wizard: folder → optional provider and key (keyring) → automatic indexing.
  - One-click or copy-paste MCP client setup.
  - Plain-language errors, each with one next action. Push-based progress, never polling.
  - Advanced settings behind "Advanced", never required.
- **Security is never weakened for simplicity.** Sandboxing, traversal protection, loopback binding and keyring secrets are always on and invisible. When the two conflict, choose the secure default and hide it.
- **Standing check** for every phase review, design and PR: "Can an average user do this without documentation?" Log any failure as a risk in `docs/04-roadmap.md`.

## Other constraints
- Build, test and check commands, and the tools they need, are in `CONTRIBUTING.md`.
- Backend fully in Rust. Use established, maintained libraries; verify versions against current sources.
- One provider interface, one config schema, one error model, one event bus across all crates.
- Selene's sources are read from a Selene checkout next to this repository (`../selene`) by `cargo xtask sync-providers` and the eval scripts; pass `--selene <dir>` (or `SELENE_ROOT` for the eval `.mjs` scripts) to use another one (Amendment 15).

## Build cadence (agents working in this repository)
- While implementing a phase, do NOT run workspace-wide `cargo build`/`test`/`release`/`dist`, and do not rebuild after each fix. Reason about correctness from the code. The only allowed exception is `cargo check -p <crate>` to confirm a risky edit.
- At the end of each phase, run ONE consolidated gate with `-j 12`: build, test, clippy `-D warnings`, fmt `--check`, `cargo deny --locked check`, `xtask check-crt`, `check-delayload`, `check-bridge-deps`. Fix all findings in one batch, then rerun the gate once.
- Build `release` once per phase and the `dist` installer once per milestone. Do not change `Cargo.toml` profiles.
- Only one heavy build at a time across all agents.
