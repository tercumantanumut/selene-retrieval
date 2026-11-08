# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- First public source release: README, contributing guide, security policy, code of conduct, issue and pull request templates.

### Removed

- The "Only list the files" folder mode ("Files only"). A folder set to it is paused after the update, so nothing in it is read, and the app asks once whether to search its contents or remove it.

## [0.1.0] - not yet released

The first beta. Windows 10 and 11, x64.

### Added

- Desktop app with a first-run wizard: choose a folder, optionally add an AI provider, connect Claude Desktop.
- Local hybrid search (meaning plus exact words) over text, Markdown, code, HTML, PDF, Word, Excel and PowerPoint files, with a bundled model and no account or API key.
- Live folder sync: new, changed and deleted files are picked up automatically, with pause and resume.
- Graphics-card acceleration through DirectML when available, with a silent fallback to the CPU.
- Optional larger and multilingual search models, downloaded only after you choose them.
- MCP server for AI apps: `context_search` and `list_folders`, plus `context_answer` with an AI provider, and `add_folder` and `sync_folder` when you turn them on.
- One-click connect for Claude Desktop (classic and Microsoft Store), Cursor and VS Code, and setup text for other MCP apps.
- AI providers with a key (stored in Windows Credential Manager), local Ollama and vLLM, and "Claude (sign in)" and "Codex (sign in)" through the vendors' own command-line tools.
- Written answers with numbered citations, and a Prompt Enhancer page in the app, both asking before anything is sent to a provider.
- App pages for search, folders and folder groups, providers, connected apps, activity and settings.
- Per-app folder access, and an optional HTTP connection on `127.0.0.1` with a password per tool.
- Administrator policies (`policy.toml` or the registry) that can switch features off.
- Per-user NSIS installer and MSI, plus offline variants that include WebView2. Not code-signed yet.
