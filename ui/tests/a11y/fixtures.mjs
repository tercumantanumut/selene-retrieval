// Fake service data for the accessibility tests: the shapes of `src/bindings.ts`, with plain
// made-up names (no real paths, secrets or client configs). Every command the UI calls has a
// default answer; a scenario overrides what it needs.

export const ID = {
  docs: "01JDOCS0000000000000000000",
  code: "01JCODE0000000000000000000",
  usb: "01JUSB00000000000000000000",
  onedrive: "01JONEDRIVE000000000000000",
  removing: "01JREMOVING000000000000000",
  groupA: "01JGROUPA00000000000000000",
  groupB: "01JGROUPB00000000000000000",
};

export function folder(over = {}) {
  return {
    id: ID.docs,
    name: "Documents",
    path: "C:\\Users\\test\\Documents",
    state: "up_to_date",
    line: "Up to date · 2,100 files",
    files_done: 2100,
    files_total: 2100,
    files_skipped: 0,
    eta_s: null,
    note: null,
    group_id: ID.groupA,
    was_files_only: false,
    online_only_skipped: 0,
    cloud_note: null,
    ...over,
  };
}

export const FOLDERS = [
  folder(),
  folder({
    id: ID.code,
    name: "Projects",
    path: "C:\\Users\\test\\Projects",
    state: "indexing",
    line: "Indexing 340 of 2,100 files · about 4 minutes left",
    files_done: 340,
    eta_s: 240,
  }),
  folder({
    id: ID.usb,
    name: "Backup drive",
    path: "E:\\Backup",
    state: "unavailable",
    line: "Can't reach this folder. The drive may be unplugged or the folder moved.",
  }),
  folder({
    id: ID.onedrive,
    name: "OneDrive",
    path: "C:\\Users\\test\\OneDrive",
    state: "up_to_date_with_skips",
    line: "Up to date · 400 files (12 skipped)",
    files_skipped: 12,
    online_only_skipped: 12,
    cloud_note: "This folder is in OneDrive.",
    note: { kind: "network", text: "This folder is on a network drive, so changes are picked up every hour." },
  }),
  folder({
    id: ID.removing,
    name: "Old notes",
    path: "C:\\Users\\test\\Old notes",
    state: "removing",
    line: "Removing… It no longer appears in search.",
  }),
];

export function uiError(over = {}) {
  return {
    title: "Background service didn't start",
    body: "Selene Retrieval couldn't start its background service.",
    action: { kind: "restart_app" },
    action_label: "Restart Selene Retrieval",
    details: "ServiceUnavailable · trace 01JTRACE",
    ...over,
  };
}

export function appView(over = {}) {
  return {
    overall: "indexing",
    summary: "Indexing 340 of 2,100 files",
    folders: FOLDERS,
    problem: null,
    tier: { active: "basic", recommended: "better", gpu: true, multilingual: false },
    reconnects: 0,
    notices: [],
    model_change: null,
    approvals: [],
    catching_up: false,
    waiting_for_fullscreen: false,
    provider_health: "ok",
    consent_request: null,
    focus_approval: null,
    restarting: false,
    index_move: null,
    policy: { managed: [] },
    index_missing: null,
    ...over,
  };
}

const settings = {
  display_name: "",
  keep_up_to_date: true,
  file_types_preset: "documents_and_code",
  include_online_only: false,
  sync_mode: "auto",
  cadence_min: 60,
  reindex_policy: "smart",
  recursive: true,
  include_extensions: [],
  exclude_globs: [],
  max_file_bytes: 10 * 1024 * 1024,
  chunk_preset: "balanced",
  chunk_size_override: null,
  chunk_overlap_override: null,
  follow_symlinks: false,
};

export function folderDetail(id = ID.onedrive) {
  const f = FOLDERS.find((x) => x.id === id) ?? FOLDERS[0];
  return {
    id: f.id,
    name: f.name,
    path: f.path,
    available: true,
    group_id: ID.groupA,
    group_name: "My folders",
    paused: false,
    last_checked_ms: Date.now() - 5 * 60_000,
    counts: { indexed: 388, pending: 0, skipped: 12, listed: 0 },
    skipped: [
      { file_id: "f1", rel_path: "Taxes\\2025.pdf", reason: "Stored only in OneDrive", retryable: false },
      { file_id: "f2", rel_path: "Scans\\big.tif", reason: "Bigger than the 10 MB limit", retryable: false },
      { file_id: "f3", rel_path: "Locked.docx", reason: "In use by another program; retried automatically", retryable: true },
    ],
    skipped_total: 12,
    settings,
    watch_note: "This folder is on a network drive, so changes are picked up every hour.",
    online_only_skipped: 12,
    cloud_note: "This folder is in OneDrive.",
  };
}

export const PROVIDER = {
  provider: "openai",
  provider_name: "OpenAI",
  key_saved: true,
  key_last4: "wxyz",
  address: null,
  check: "working",
  last_error: null,
  model: null,
  default_model: "gpt-example",
  models: [{ id: "gpt-example", name: "gpt-example" }],
  remote_embeddings: false,
  remote_rerank: false,
};

const NO_PROVIDER = { ...PROVIDER, provider: null, provider_name: null, key_saved: false, key_last4: "", default_model: null, models: [] };

export const SEARCH = {
  files: [
    {
      path: "C:\\Users\\test\\Documents\\lease.md",
      rel_path: "lease.md",
      file_name: "lease.md",
      folder_id: ID.docs,
      folder_name: "Documents",
      hits: [
        { line_start: 3, line_end: 9, snippet: "The lease agreement starts on 1 May.", scores: { rrf: 0.03, dense: 0.81, bm25: 12.1, rerank: null } },
        { line_start: 20, line_end: 24, snippet: "Rent is due on the first.", scores: { rrf: 0.02, dense: 0.7, bm25: 4.2, rerank: null } },
      ],
    },
  ],
  hits: 2,
  asked: 50,
  folders: 1,
  no_folders: false,
  no_good_match: false,
};

export const ANSWER = {
  text: "The lease starts on 1 May [1].\n\nRent is due on the first of each month [1].",
  sources: [
    { n: 1, path: SEARCH.files[0].path, rel_path: "lease.md", file_name: "lease.md", folder_name: "Documents", line_start: 3, line_end: 9 },
  ],
  provider_name: "OpenAI",
  model: "gpt-example",
  rounds: 0,
  no_good_match: false,
  no_folders: false,
};

function serviceClient(over = {}) {
  return {
    client_id: "claude_desktop",
    kind: "claude_desktop",
    name: "Claude Desktop",
    scope: { mode: "all" },
    last_used_ms: Date.now() - 3_600_000,
    sessions_open: 1,
    token_last4: null,
    ...over,
  };
}

/** CLI sign-in states (M5-C). */
export const CLAUDE_SIGNED_OUT = {
  provider: "claudecode",
  state: "signed_out",
  signed_in: false,
  email: null,
  plan: null,
  tool: "Claude Code",
  install_url: "https://claude.com/claude-code",
  check_failed: false,
};
export const CLAUDE_SIGNED_IN = { ...CLAUDE_SIGNED_OUT, state: "signed_in", signed_in: true, email: "test@example.com", plan: "Max" };
export const CODEX_NOT_INSTALLED = {
  provider: "codex",
  state: "not_installed",
  signed_in: false,
  email: null,
  plan: null,
  tool: "Codex",
  install_url: "https://developers.openai.com/codex/cli",
  check_failed: false,
};

/** Default answers for every command the UI calls (`cmd` = the Tauri command name). */
export function defaults(view) {
  return {
    get_state: view,
    get_claude_status: { claude: "connected", snippet: "{\"mcpServers\":{}}", changed_externally: false },
    get_about: { version: "0.5.0", data_dir: "C:\\Users\\test\\AppData\\Local\\SeleneRetrieval", start_with_windows: true, autostart_decided: true },
    get_advanced_settings: { use_graphics_card: true, allow_app_folder_requests: true, allow_app_folder_checks: false, restart_needed: true },
    get_provider_status: PROVIDER,
    get_providers: [
      { id: "openai", name: "OpenAI", auth: "api_key", available: true, disabled_reason: null, default_address: null, model_hint: "gpt-example", key_url: "https://example.invalid/keys" },
      { id: "claudecode", name: "Claude (sign in)", auth: "oauth", available: true, disabled_reason: null, default_address: null, model_hint: null, key_url: "https://claude.com/claude-code" },
      { id: "ollama", name: "Ollama", auth: "local", available: true, disabled_reason: null, default_address: "http://127.0.0.1:11434", model_hint: null, key_url: null },
      { id: "codex", name: "Codex (sign in)", auth: "oauth", available: true, disabled_reason: null, default_address: null, model_hint: null, key_url: "https://developers.openai.com/codex/cli" },
      { id: "antigravity", name: "Antigravity", auth: "api_key", available: true, disabled_reason: null, default_address: null, model_hint: null, key_url: null },
    ],
    // "Claude (sign in)" / "Codex (sign in)" (M5-C, crates/app/src/cli_signin.rs): Claude Code
    // installed but not signed in, Codex not installed. Never a token.
    cli_signin_status: ({ provider }) => (provider === "codex" ? CODEX_NOT_INSTALLED : CLAUDE_SIGNED_OUT),
    cli_signin_start: ({ provider }) => ({
      status: { ...(provider === "codex" ? CODEX_NOT_INSTALLED : CLAUDE_SIGNED_OUT), state: "signing_in" },
      started: provider !== "codex",
    }),
    get_consents: [
      { purpose: "answer", allowed: true },
      { purpose: "enhance", allowed: false },
    ],
    get_answers_settings: { answers_enabled: true, deeper_answers: false, max_rounds: 2 },
    get_clients: {
      apps: [
        { app: "claude_desktop", name: "Claude Desktop", state: "connected", snippet: "{}", changed_externally: false, last_used_ms: Date.now() - 60_000 },
        { app: "cursor", name: "Cursor", state: "not_connected", snippet: "{}", changed_externally: false, last_used_ms: null },
        { app: "vs_code", name: "VS Code", state: "cant_read", snippet: "{}", changed_externally: false, last_used_ms: null },
      ],
      other_snippet: "{\n  \"mcpServers\": {\n    \"selene-retrieval\": { \"command\": \"sr-bridge.exe\" }\n  }\n}",
    },
    get_service_clients: {
      clients: [
        serviceClient(),
        serviceClient({ client_id: "cursor", kind: "cursor", name: "Cursor", sessions_open: 0, scope: { mode: "groups", group_ids: [ID.groupA] } }),
        serviceClient({ client_id: "http-01", kind: "http", name: "My script", sessions_open: 0, token_last4: "wxyz", last_used_ms: null }),
      ],
      http: { enabled: true, running: true, url: "http://127.0.0.1:47011/mcp", problem: null, tools: 1 },
    },
    get_groups: [
      { id: ID.groupA, name: "My folders", is_default: true, folder_ids: [ID.docs, ID.code] },
      { id: ID.groupB, name: "Work", is_default: false, folder_ids: [ID.onedrive] },
    ],
    get_activity: {
      rows: [
        { id: 3, ts_ms: Date.now() - 60_000, icon: "up_to_date", text: "“Documents” is up to date · 2,100 files", folder_id: ID.docs, to_skipped: false },
        { id: 2, ts_ms: Date.now() - 120_000, icon: "skipped", text: "12 files skipped in “OneDrive”", folder_id: ID.onedrive, to_skipped: true },
        { id: 1, ts_ms: Date.now() - 86_400_000, icon: "app", text: "Claude Desktop used Selene Retrieval for the first time", folder_id: null, to_skipped: false },
      ],
      next_before_id: 1,
    },
    get_egress: {
      rows: [{ id: 1, ts_ms: Date.now() - 600_000, provider_name: "OpenAI", what: "questions and matching file text", size: "12 KB", requested_by: "This app" }],
      next_before_id: null,
      providers: [{ id: "openai", name: "OpenAI" }, { id: "ollama", name: "Ollama" }],
    },
    get_folder_detail: (args) => folderDetail(args?.folderId),
    get_index_location: { path: "C:\\Users\\test\\AppData\\Local\\SeleneRetrieval", is_default: true, size: "1.4 GB", free: "120 GB" },
    get_licenses: [{ name: "LICENSE-selene-retrieval", text: "Example licence text." }],
    search: SEARCH,
    ask: ANSWER,
    update_folder_settings: (args) => ({ reindex: false, detail: { ...folderDetail(args?.folderId), settings: args?.settings } }),
    check_folder_settings: [],
    folder_change_impact: "reread",
    get_diagnostics: "diagnostics",
    "plugin:dialog|open": null,
  };
}

export const NO_PROVIDER_STATUS = NO_PROVIDER;
