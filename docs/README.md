# Design records

The files in `docs/` and `design-docs/` are dated design records written while Selene Retrieval was built. They explain why the code is the way it is. They are not user guides: for installing and using the app, see the [README](../README.md).

How to read them:

- Each file has a status line from the day it was written. The code has moved on since; the [roadmap](04-roadmap.md) has the latest state.
- "Owner" means the maintainer of this repository.
- The "Amendments" in [00-review-and-decisions.md](00-review-and-decisions.md) are the recorded design decisions. Other docs and code comments cite them by number (for example "Amendment 3.2").
- "P1" and "P2" are the [Product Principles](product-principles.md). Older docs say they are in `README.md`, where they used to live.
- Milestone codes (M0 to M6) are defined in the [roadmap](04-roadmap.md).
- Docs written before 2026-11-08 use the old MCP tool names (`search`, `answer`, `read_file`, `grep`, `get_status`, `enhance_prompt`). Amendment 16 in [00-review-and-decisions.md](00-review-and-decisions.md) records the current tools: `context_search`, `list_folders`, `context_answer`, and `add_folder` / `sync_folder` when the user turns them on.
- Some docs and code comments cite internal working notes that are not published, for example `docs/research/m1-b-extract.md`. The decisions they record are summarised in [00-review-and-decisions.md](00-review-and-decisions.md) and [04-roadmap.md](04-roadmap.md).

## docs/

| File | What it covers |
|---|---|
| [README.md](README.md) | This index. |
| [product-principles.md](product-principles.md) | The Product Principles (P1 simplicity, P2 the standing review check). |
| [00-review-and-decisions.md](00-review-and-decisions.md) | The plan review, the open decisions of that time and the numbered Amendments (design decisions). |
| [02-architecture.md](02-architecture.md) | Crate layout, processes, data flow, the catalog schema, shared contracts, security, hardware tiers. |
| [04-roadmap.md](04-roadmap.md) | Milestones with exit criteria and results, open items, and the risks table. |
| [06-output-efficiency-plan.md](06-output-efficiency-plan.md) | Making the MCP search and answer results shorter for AI apps (the tools are now `context_search` and `context_answer`). |
| [07-default-folder-plan.md](07-default-folder-plan.md) | "Create a folder for me": the default folder in Documents. |
| [08-broad-query-ranking.md](08-broad-query-ranking.md) | Ranking for broad "how does X work" questions. |
| [research/m1-wave2-integration-notes.md](research/m1-wave2-integration-notes.md) | Integration requirements collected while building the first engine. |
| [research/m4-rpc-contract.md](research/m4-rpc-contract.md) | The contract between the desktop app and the background service (methods and events). |

## design-docs/

| File | What it covers |
|---|---|
| [00-design-decisions.md](../design-docs/00-design-decisions.md) | Design decisions for the app (purpose, visual language, words, interaction), each with rationale and rejected alternatives. |
| [design-system.md](../design-docs/design-system.md) | The app's design system: tokens, layout, icons, components, status names, accessibility and wording rules. |
