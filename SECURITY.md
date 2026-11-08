# Security policy

Selene Retrieval reads your private files and hands parts of them to AI apps. We take security reports seriously.

## Supported versions

| Version | Supported |
|---|---|
| The latest release | Yes |
| `main` (beta) | Yes |
| Older releases | No |

## Reporting a vulnerability

**Do not open a public issue or pull request for a security problem.**

Report it privately through GitHub's private vulnerability reporting:
https://github.com/tercumantanumut/selene-retrieval/security/advisories/new

Only the maintainers can see the report.

Please include:

- What the problem is and what an attacker could do with it.
- The Selene Retrieval version (**Settings** → **Help**) and your Windows version.
- Steps to reproduce it, or a proof of concept.
- Which part is affected, if you know (installer, app, background service, MCP bridge, HTTP connection, document extractor).
- Whether the problem is already public.

## What happens next

This is a small project with one maintainer, so responses are best effort. We aim to send a first reply within 7 days.

## Security model

These protections are always on. The user does not need to set anything.

- **Local by default.** Indexing and search run on the PC with a bundled model. Selene Retrieval sends no file text to an online AI service unless the user adds one and allows each purpose. Passages it gives a connected AI app are handled by that app (see the limits below). Larger models are downloaded only when the user asks, and only pinned, hash-checked files. There is no telemetry.
- **A small, read-only tool surface.** AI apps get passages through `context_search` and `context_answer` and the folder list through `list_folders`. They can't open or browse files. `add_folder` and `sync_folder` are listed only when the user turns them on, and `add_folder` still needs the user's approval in the app. A tool that is not listed answers like an unknown tool.
- **Named pipe for the current user only.** AI apps reach the background service through a named pipe whose access list allows only the current Windows user. Low-integrity processes and connections from other computers are refused.
- **Optional HTTP, loopback only.** The HTTP connection for other local tools is off by default. When on, it listens on `127.0.0.1` only, checks the `Host` and `Origin` headers, and needs a separate random password (a bearer token) for each tool.
- **Secrets in Windows Credential Manager.** API keys and HTTP passwords are stored there, never in a settings file.
- **Folder sandbox and path checks.** File reads (indexing, the watcher, the reads an answer makes) stay inside the folders the user added. `..`, links and junctions that lead outside, alternate data streams and device names are refused. Files that usually hold secrets (such as `.env` or `.pem`) are never read or indexed.
- **Sandboxed document extractor.** Documents such as PDF and Office files are parsed in a separate process inside a Windows AppContainer with no capabilities, so it has no file or network access. It runs in a job object with a memory limit and kill-on-close, and gets the file's bytes, never its path. If Windows can't create the AppContainer, the extractor falls back to a restricted low-integrity token and the app shows "File reader running with reduced protection".
- **Signature-checked DirectML.** The DirectML library for the graphics card is loaded by absolute path, only after its Microsoft Authenticode signature is checked.

## Out of scope and known limits

- **Programs running as the same Windows user.** They can read the same files and can connect to the background service like an AI app does. Selene Retrieval does not protect against malware already running as you.
- **The unsigned beta installer.** Releases are not code-signed yet. Windows SmartScreen warns about them. Download only from this repository's Releases page and check the SHA-256 hash.
- **What an AI app does with text it receives.** Text returned to an AI app becomes part of that app's conversation, and the app may send it to its own AI service.
- **Online providers the user turns on.** Text the user allows to be sent to an AI provider is handled under that provider's terms. "Claude (sign in)" and "Codex (sign in)" run the vendors' own command-line tools.

Reports about these limits are still welcome if you find a way to make them safer without making the app harder to use.
