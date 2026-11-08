# Product Principles

These principles apply to every design, library choice, milestone and review in this project. Other docs cite them as P1 and P2. Older docs say they are in `README.md`, where they used to live.

## P1. Simplicity and ease of use for the average user (core)

A non-technical user can download, install, set up and use the product from first launch, **without documentation**.

1. **One installer, no prerequisites.** A single installer (NSIS per-user exe, or an MSI for enterprise). Releases are not code-signed yet; signing is planned. No terminal, no manual config-file edits and no separate runtime to install: no Python, Node, protoc or VC++ redistributables. The installer handles WebView2 if it is missing.
2. **Sensible defaults everywhere.** A local embedding model and local-only processing work out of the box, with no API key. Optional features stay off until the user turns them on. This includes the secondary LLM iteration pass, remote embeddings and the MCP `add_folder` tool.
3. **First-run wizard.** The Tauri app guides the user through these steps:
   1. Pick a folder.
   2. Optionally choose a provider and paste an API key. The key is validated live and stored in Windows Credential Manager via `keyring`.
   3. Indexing starts automatically, with visible progress.

   Skipping the provider step still gives the user working local search.
4. **One-click MCP client setup.** Supported clients get a "Connect" button that writes their config. Every client also gets a generated copy-paste snippet for stdio or streamable HTTP, with the token already filled in.
5. **Plain language.** UI text and errors say what happened and give **one clear next action**, for example "Retry", "Open settings" or "Choose a smaller folder". Raw error codes appear only under "Details". Progress is pushed live through events and never polled.
6. **Advanced stays advanced.** Tuning lives behind an "Advanced" section that basic use never needs. This covers chunk sizes, RRF k, rerankers, DirectML, ports and config-file overrides.
7. **Security is invisible and never weakened.** Sandboxed folders, path-traversal protection, loopback-only binding, keyring secrets and bearer tokens are always on and need no user action. **Where simplicity and security conflict, choose the secure default and hide it from the user.** Never trade security away for convenience.

## P2. Standing review check

Every phase review, design doc and PR asks:

- [ ] **Can an average user do this without documentation?**
- [ ] Does it work with zero configuration on a fresh Windows 10/11 machine?
- [ ] Are new options defaulted sensibly and kept out of the basic flow?
- [ ] Does every user-visible error have plain wording and a next action?
- [ ] Are security defaults unchanged and invisible to the user?

A design that fails any item is logged as a risk in `docs/04-roadmap.md` (the risks table), together with its mitigation.
