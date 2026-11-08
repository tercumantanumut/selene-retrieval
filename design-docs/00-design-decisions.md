# 00. Design decisions

Status: **FINAL** for the design package. Each record has: **Decision / Rationale / Alternatives rejected / Source**.
Records marked **[New]** are design choices made here, because the sources specify the feature but not its UI. Each
of them that leaves a real choice open points to an open question (OQ-n). The full list of open questions is in the
design package's README, which is not published.

No record contradicts `docs/00-review-and-decisions.md` (Plan v1 + Amendments 1–5) or the product principles P1/P2
(`../docs/product-principles.md`). Where they differ from the M2 code, the record says so; the full list of differences
is in the same unpublished README.

---

## A. Purpose, users, platform

### D-1 Purpose
- **Decision:** the app's one job is to let a person choose folders on their PC and make them searchable by AI apps
  (Claude Desktop first), privately and with no setup. From M4 the person can also search them inside the app. The
  app is the status and control surface for an invisible background service.
- **Rationale:** P1 ("download, install, set up and use … without documentation"). The installer's own description is
  "Search your folders from Claude and other AI apps" [Code `tauri.conf.json`]. The M2 success test is "download →
  Claude answers from the user's own file in under 5 minutes" (internal user-test notes, not published).
- **Alternatives rejected:** a search-first desktop app (search UI is M4 scope, 04); a developer tool with CLI setup
  (R-UX1, U1, U2).
- **Source:** `docs/product-principles.md` P1; 04 M2/M4 rows; (internal user-test notes, not published).

### D-2 Target users
- **Decision:** two audiences, one UI.
  1. **Primary: the average, non-technical Windows 10/11 user.** Uses Claude Desktop, doesn't know what MCP is, has
     never edited a JSON file, may have a laptop with no GPU or only a built-in one.
  2. **Secondary: MCP-client users** (Cursor, VS Code, other MCP apps, local tools over HTTP). They get the
     copy-paste setup text and, in M4, Advanced → Connected apps.

  Basic surfaces are designed only for (1). Everything (2) needs sits behind "Copy setup text" or Advanced.
- **Rationale:** P1.4 and P1.6; the U-list in 00 §0.B; the user test uses three non-technical users (Amendment 5.4).
- **Alternatives rejected:** separate "simple" and "pro" modes (two UIs to test; Advanced already covers it).
- **Source:** `docs/product-principles.md` P1; 00 §0.B U2/U4/U15/U18; Amendment 5.4.

### D-3 Platform: Tauri 2 + WebView2, Windows-first
- **Decision:** a Tauri 2 desktop window rendered by WebView2 (React 19, Vite, TanStack Router/Query, Tailwind 4,
  shadcn-style components on `radix-ui`). Windows conventions apply:
  - Windows' own folder picker [Code `pickFolder`];
  - a tray icon next to the clock;
  - toast notifications through the OS;
  - the window hides to the tray on close;
  - single instance (a second launch focuses the window) [Code].

  Strict CSP, no shell/fs/http/opener permissions (capabilities: event listen/unlisten and `dialog:allow-open` only).
- **Rationale:** 01/02 crate table (`sr-app` = Tauri); m2-a-app.md §2 (security); P1.7.
- **Alternatives rejected:** a native Win32/WinUI app (outside the decided stack); a browser UI on localhost (no TCP
  port by default, 00 decision 6).
- **Consequences for design:** no web fonts, no remote images (`img-src 'self' data:`), no links that open a browser
  unless an opener capability is added later (OQ-40). Memory-history routes, so there is no URL bar or back button.
- **Source:** 02 §2.1, §2.6 "Desktop"; m2-a-app.md §2; `tauri.conf.json`.

### D-4 Offline-first and local-only by default
- **Decision:** every Basic flow works with no network:
  - the bundled model (bge-small-en-v1.5 fp32, Amendments 3.4/4.26) indexes and searches offline;
  - the only network actions are explicit, consented clicks: a search model download ([Use it]), a provider key (M3)
    and an update check;
  - offline failures use code 27 `Offline` ("You're offline · Selene Retrieval couldn't reach {host}. Local search
    still works.").
- **Rationale:** P1.2; Amendment 1 "nothing downloads without a click"; M1/M2 zero-config checks (0 network
  endpoints).
- **Alternatives rejected:** downloading a better model on first run (fails offline, U9).
- **Source:** 00 Amendment 1, 3.4, 4.26; 02 §2.5 codes 27–31; 04 M1/M2 rows.

### D-5 Zero-config first run; "first run" = no folders
- **Decision:** the app opens on the wizard whenever the service has no folders. There is no first-run state file
  [Code `Overview.tsx` redirect, m2-a §9.1]. No account, key, setting or download is needed.
- **Rationale:** P1.3 and the P2 zero-configuration item.
- **Alternatives rejected:** a "first launch" flag (it gets out of sync after uninstall/reinstall with a kept index).
- **Source:** m2-a-app.md §9; `docs/product-principles.md` P1.3.

### D-6 Per-user install, no admin
- **Decision:** the NSIS installer installs to `%LOCALAPPDATA%\Selene Retrieval` for the current user, with no UAC
  prompt and no options. Pages: Welcome → progress → Finish ("Open Selene Retrieval", checked). Uninstall: one
  confirm page with one checkbox (D-33).
- **Rationale:** P1.1, U7; m2-i §3, §7 P2 check.
- **Alternatives rejected:** per-machine MSI for consumers (an MSI comes in M5 for enterprise only).
- **Source:** m2-i-installer.md; `installer/nsis/selene-retrieval.nsi`.

### D-7 Unsigned beta; SmartScreen guidance outside the product
- **Decision:** the M2 beta ships unsigned (Amendment 5.2). SmartScreen's "Windows protected your PC" can't be styled
  or avoided. The design answer is a help panel on the **download page** and in the user-test kit (internal user-test notes, not published): "More info → Run
  anyway", with two real screenshots. Page 01 specifies that panel (OQ-8). Signing returns before GA (M5).
- **Rationale:** R-UX6 (H/H); Amendment 5.2; (internal user-test notes, not published).
- **Alternatives rejected:** a self-signed certificate (no SmartScreen benefit, and a fake trust signal).
- **Source:** 00 Amendment 5.2; 04 R-UX6.

### D-8 The background service is invisible; the app is a client
- **Decision:** the UI never says "daemon". It calls the service "the background service" only in error and Advanced
  text ("Background service didn't start", "Reconnect to the background service").
  - Status reaches the UI as a plain summary line.
  - Closing the window keeps the app in the tray.
  - Re-attaching after a service restart happens without a reload [Code].
- **Rationale:** U6, U25, R-UX8; m2-a §6 (re-attach verified).
- **Alternatives rejected:** a "service status" page (jargon, R-UX4).
- **Source:** 00 U6; 02 §2.2; m2-a-app.md.

---

## B. Visual language

### D-9 Neutral, quiet visual language; follow Windows light/dark
- **Decision:** the baseline is the shadcn-style neutral palette in `ui/src/index.css`:
  - a cool-grey foreground (hue 265);
  - one deep-indigo primary (light `#233459`, dark `#B1C4EC`);
  - green / amber / red reserved for state.

  Light and dark follow Windows through `prefers-color-scheme`, with no in-app theme setting (OQ-1). The Windows accent
  colour is not adopted (OQ-2). Cards on a plain background, generous whitespace, one primary button per card.
- **Rationale:** existing tokens [Code]. A calm look suits a background utility whose ideal state is "Up to date".
  Following Windows avoids a setting (P1.2).
- **Alternatives rejected:** Fluent/WinUI mimicry (not in the stack, and a WebView2 imitation would read as fake); a
  branded colourful theme (no brand guidelines exist in the sources, OQ-5).
- **Source:** `ui/src/index.css`; m2-a-app.md §2 UI.

### D-10 Typography
- **Decision:** the system stack `"Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif` [Code], Tailwind
  sizes 12/14/16/18/20 px, weights 400/500/600. Sentence case. Sizes in rem so Windows text scaling applies.
- **Rationale:** the code defines it; it matches Windows 11 (Segoe UI Variable) and falls back on Windows 10.
- **Alternatives rejected:** bundled fonts (CSP `font-src 'self'`, extra size, R25).
- **Source:** `ui/src/index.css`; design-system DS-1.2.

### D-11 Colour semantics: one "attention" style for every error
- **Decision:**
  - every user-facing error, notice and problem uses the **amber warning** panel (ErrorPanel) [Code];
  - **red** is used only for destructive confirmation buttons ("Remove");
  - **green** means done/connected;
  - **primary** means working/active.
- **Rationale:** P1.5 frames errors as "what happened + one next action", not alarms. The code renders every
  `UiError` with one panel style.
- **Alternatives rejected:** red for failures and amber for warnings. The 37 codes don't carry a severity, and P1
  avoids alarm. OQ-3 keeps the question open.
- **Source:** `ErrorPanel.tsx`; 02 §2.5.

### D-12 Iconography
- **Decision:** lucide icons, 16 px inline and 20 px for state, always paired with text, decorative icons
  `aria-hidden`. The state icon set is fixed in DS-5. Four tray icons with distinct shapes.
- **Source:** `FolderCard.tsx`, `tray.rs`; design-system DS-3.

---

## C. Words

### D-13 Tone of voice
- **Decision:** plain, calm, second person, short sentences. Say what happened and what to do; reassure about privacy
  where relevant ("Your files are not uploaded anywhere."). Examples that set the tone [Code]:
  - "You can keep working. Indexing continues in the background, even if you close this window."
  - "One click lets Claude search the folders you added. Nothing else in Claude's settings changes."
  - "Diagnostics never include your files, searches, folder names or keys."
- **Rationale:** P1.5; R-UX4.
- **Source:** `ui/src`; `crates/common/src/error/messages.rs`.

### D-14 Plain-language errors from one renderer
- **Decision:** every error the user sees is a `UiError` built from `sr_common`'s single `UserMessage` renderer:
  - **title + body + one action button + "Details"** (folded; shows `"<Code> · trace <id>"`);
  - the 37 codes and their copy are fixed (02 §2.5, `messages.rs`);
  - the UI never writes its own error text. The only exceptions are the app-local errors in `crates/app`
    (Claude connect, autostart, updater, invalid folder path); 15 and 01-flows list them.
  - Action labels come from `SuggestedAction::label()`: Retry, Try again, Open settings, Re-enter key, Add a
    provider, Allow, Choose a folder, Choose a subfolder, Review, View skipped files, Free up space, Restart Selene
    Retrieval, How to restart, Reconnect, Copy diagnostics.
  - The M2 app relabels RestartClient to "How to restart Claude" and maps not-yet-built screens to "Open settings"
    [Code `dto.rs`].
- **Rationale:** P1.5; 02 §2.5 "UserMessage::from(&Error) is the only renderer"; a unit test covers all 37 codes for
  a non-empty title/body/label and no "daemon".
- **Alternatives rejected:** per-page error copy (drifts; can't be localised in one place).
- **Source:** 02 §2.5; `dto.rs`; `messages.rs`.

### D-15 Vocabulary
- **Decision:** fixed user words:

  | Concept | User word |
  |---|---|
  | Tiers | Basic / Better / Best search |
  | Execution provider | "Use graphics card" |
  | MCP config | "setup text" |
  | Collections | "Folder groups" (Advanced only, OQ-48) |
  | Daemon | "background service" |
  | Re-embed | "Improving search…" |
  | Purge | "Removing…" |
  | Metered | "this connection may cost money (metered)" |

  Folder and model states use DS-5 only.
- **Source:** 00 Amendment 1 P2 check ("no tier jargon"), U6, U15; 02 §2.5 table; `state.rs`.

---

## D. Interaction model

### D-16 Push, never poll
- **Decision:** the UI shows state only from pushed events. The app emits `StateChanged` (the whole `AppView`,
  coalesced to ≤ ~7/s) and `ClaudeConfigChanged`. TanStack Query has `staleTime: Infinity` and no refetch. ESLint
  bans `setInterval`, `setTimeout` and `refetchInterval` [Code].
  - Designs therefore must never need a timer.
  - Countdowns (the approval expiry, DS-6) are derived from the push of the next state, or rendered as static text
    ("Expires in about 10 minutes"), not ticking seconds.
  - Spinners animate in CSS.
- **Rationale:** P1.5 ("Progress is pushed live through events and never polled"); W8; m2-a §6 (zero status requests
  during 105 s of indexing).
- **Source:** `docs/product-principles.md` P1.5; 02 §2.5 Events; `ui/src/lib/app.ts`; `tests/no-polling.test.mjs`.

### D-17 Accessibility target
- **Decision:** WCAG 2.2 AA on every page now (the formal pass is M5): keyboard, screen reader, reduced motion,
  Windows contrast themes, 200 % text. The token and motion changes needed are in design-system DS-1.1/1.6/6 (OQ-11).
- **Source:** user request; 04 M5 "accessibility pass".

### D-18 Basic vs Advanced
- **Decision:** Settings has Basic sections in cards, plus one collapsed "Advanced" disclosure [Code]. Basic use never
  needs Advanced (user test S4).
  - Fields follow the schema tags `x-tier` / `x-section` / `x-restart` (02 §2.5).
  - In M2 the page is hand-written. From M4 it is generated from the JSON schema (OQ-23).
  - A setting that needs a restart says so: "This takes effect the next time Selene Retrieval starts." [Code]
- **Source:** `docs/product-principles.md` P1.6; 02 §2.5; `Settings.tsx`; `config/mod.rs`.

### D-19 GPU on by default, invisible
- **Decision:** there is no GPU UI in Basic. Advanced has one switch, "Use graphics card" (on = Auto), with the help
  line "Indexes faster. Turn off if other apps slow down while indexing. Applies after a restart." [Code]. GPU faults
  never surface as errors; they appear only in diagnostics (`ep.changed`).
- **Source:** Amendment 2; 02 §2.5 ("GPU problems have no user-facing code").

### D-20 Search model tiers: silent detection, one recommendation line, the click is the consent
- **Decision:**
  - Tier detection is silent.
  - Settings → "Search quality" shows one read-only line, "Recommended for your computer: Better search ({size}
    download)", plus [Use it], only when the recommended tier is above the active one.
  - [Use it] is the consent to download. Nothing downloads without it.
  - Progress, metered pause ("Download anyway"), Stop and failures follow DS-5.4.
  - A dismiss ("Not now") is added (OQ-21).
  - Advanced → "Search model" offers Basic/Better/Best, with a non-blocking warning above the recommendation.
- **Source:** Amendment 1; 02 §2.5, §2.8; m2-g §6; `Settings.tsx`; R-UX16/17.

### D-21 Features stay hidden until they can be used
- **Decision:** UI for things that need a provider (Answers, Deeper answers, Prompt Enhancer, "Ask" in Search) stays
  hidden until a provider exists, mirroring the MCP rule (U17). Approval cards appear only when "Let connected apps ask
  to add folders" is on and a request is pending.
- **Source:** 00 §0 change 16, U17; 02 §2.5 "Answers (visible when a provider exists)".

### D-22 Security stays invisible and is never weakened
- **Decision:** refusals are explained, never overridable.
  - Dangerous roots → "This folder can't be added" + [Choose a subfolder], which reopens the picker inside the refused
    folder [Code].
  - A non-system drive root is allowed after a plain confirm (U11).
  - Overlaps → "Already included".
  - Private files → "This file is private".
  - There are no "force" switches, no security toggles in Basic, and the Claude settings file is never changed when it
    can't be parsed.
- **Source:** `docs/product-principles.md` P1.7; 00 U11, U5; 02 §2.6; m2-a §3.

### D-23 Basic UI = folders only
- **Decision:** the user sees folders, not collections. The default collection "My folders" is implicit. Collections
  ("Folder groups") and per-client scopes live in Advanced (M4).
- **Source:** 00 U15; 02 §0.A Q3.

### D-24 In-app search arrives in M4; before that, search is MCP-only
- **Decision:** M2–M3 have no Search page. Search happens in Claude (MCP), which renders "Nothing closely matched
  “…” in the user's folders." M4 adds the Search page (page 26) with the same no-match wording.
- **Rationale:** the M4 page list (04) and the M2 tester ("`no_good_match` has no app surface in M2 (no search page;
  MCP renders it)"). OQ-6 asks whether an earlier minimal search is wanted.
- **Source:** 04 M4 row; m2-test-report.md §1 item 1c; m2-r §3.

### D-25 [New] Navigation for the full app (M4)
- **Decision:** the header nav holds Overview · Folders · Search · Connected apps · Settings, plus "More" (Prompt
  Enhancer, Providers & Keys, Activity). Below 760 px it collapses into a Menu button. The nav stays hidden during the
  wizard [Code].
- **Rationale:** the 04 M4 page list has 8 pages. At the 640 px minimum width eight links don't fit. The five
  everyday pages stay visible; pages that need a provider follow D-21.
- **Alternatives rejected:** a left sidebar (takes 200+ px of a 640 px minimum window); tabs inside Settings for
  everything (buries Search and Folders).
- **Source:** 04 M4; `tauri.conf.json` min width. OQ-36.

### D-26 [New] When to use a dialog
- **Decision:**
  - inline confirm for one-consequence actions (remove a folder) [Code pattern];
  - a modal dialog only where the user must read something before data leaves the computer (M3 consent), or for a
    long blocking operation (M4 moving the index);
  - never a modal for status, recommendations or errors (R-UX16 "never a modal").
- **Source:** `FolderCard.tsx`; 04 R-UX16; 00 U16.

### D-27 One status vocabulary
- **Decision:** the folder, overall, model, Claude and approval states and their copy are defined once (DS-5) and
  reused verbatim by every page, the tray and notifications.
- **Source:** `state.rs`, `tray.rs`, `ClaudeConnect.tsx`; user requirement "status names identical everywhere".

### D-28 Progress shows real numbers, estimates in words
- **Decision:**
  - determinate bars when totals are known ("Indexing 340 of 2,100 files · about 4 minutes left");
  - indeterminate while scanning;
  - estimates in words ("a minute or two", "about 40 minutes");
  - always reassure that search works meanwhile ("Search works while it runs.").
  - Very big folders (≥ 500k files) get [Choose a smaller folder] / [Add it anyway] (U12).
- **Source:** `estimate.rs`, `state.rs`; 00 U12; R-UX14/R-UX20.

### D-29 Connect writes the config; a copy-paste fallback is always there
- **Decision:**
  - one button writes Claude Desktop's config (Squirrel and MSIX, both if both exist);
  - every state has "Copy setup text";
  - after connecting, one sentence tells the user to restart Claude;
  - a file watcher keeps the status true;
  - other MCP apps get setup text (M2) and per-client writers (Cursor, VS Code, M4).
- **Source:** 00 U2/U4/U5; m2-a §3; R-UX3/21/23.

### D-30 Folder requests from AI apps are approved only in our UI
- **Decision:**
  - off by default (Advanced: "Let connected apps ask to add folders");
  - a request shows an ApprovalCard with [Allow] / [Don't allow];
  - it expires after 10 min and is never auto-approved;
  - MCP elicitation is not used.
- **Source:** 00 §0.A Q5; m2-w §2 Approvals; `ApprovalCard.tsx`; R-UX12/29/30.

### D-31 Notifications only for new problems while hidden
- **Decision:** a Windows notification appears only when the overall state **becomes** `problem` while the window is
  hidden [Code `notify_problem`]: "{problem title}" / "{body} Open Selene Retrieval to fix it.", or "A folder needs
  your attention. Open Selene Retrieval to see what happened." Everything else is in-app (OQ-26). Controlled by
  `notify_on_problems` (Basic, not yet in the UI: OQ-20).
- **Source:** `main.rs`; m2-a §9.4; `config/mod.rs`.

### D-32 [New copy] Removing a folder: "stop searching", never "delete files"
- **Decision:**
  - The confirm says "Stop searching this folder? Your files stay where they are." [Code]. **Add** "It disappears
    from search right away." (OQ-19).
  - While the purge runs, the row shows `removing` ("Removing… It no longer appears in search."), until
    `PurgeCompleted` (OQ-18).
  - The 15-minute physical-removal SLA is not shown to users. It is an engineering guarantee; Activity (M4) can
    record it.
- **Source:** 00 §0.A Q7 ("Removing…" → "Removed"); `FolderCard.tsx`; `state.rs` (PurgeCompleted removes the row).

### D-33 Uninstall keeps the search index unless asked
- **Decision:**
  - one checkbox on the confirm page, "Also delete my search index (your own files are never touched)", unticked by
    default;
  - Claude configs: only our entry is removed;
  - the scheduled task, Run keys, AppContainer profile and WebView2 data are always removed.
- **Source:** 00 U22; m2-i §3; `.nsi`.

### D-34 [New] M3 wizard: the provider step is optional and comes after indexing has started
- **Decision:** from M3 the wizard has 4 steps: 1 Choose a folder → 2 Setting up search (indexing starts at once) →
  **3 "Get written answers (optional)"** → 4 Use your folders in Claude. Step 3's primary button is "Skip for now";
  adding a key is the secondary path.
- **Rationale:** P1.3 lists the optional provider step, and indexing must start automatically. Putting the step after
  the pick keeps indexing time running while the user decides, and keeps the 5-minute M2 path intact for users who
  skip. Answers need a provider; search does not.
- **Alternatives rejected:** a provider step before indexing (delays indexing); no wizard step at all (contradicts
  P1.3).
- **Source:** `docs/product-principles.md` P1.3; `Wizard.tsx` header comment ("No provider step in M2"); 04 M3 row. OQ-9.

### D-35 [New] Consent before data leaves the computer
- **Decision:** the first time a purpose needs a remote provider, a dialog asks for that purpose only. It uses the
  plain purpose words from `Purpose::plain()` ("the text of your files", "your search text", "questions and matching
  file text", "your prompt"…). The remote-embeddings purpose gets the stronger wording "The text of your files will be
  sent to {provider}". Choices: [Allow] / [Don't allow]. A refused purpose shows code 26 `PrivacyBlocked` with
  [Allow] when retried.
- **Source:** 00 U16; 02 §2.5 PrivacyGate, code 26; `error/mod.rs` `Purpose::plain`; R-UX13. OQ-34.

### D-36 [New] In-app search results (M4)
- **Decision:** a single search box. Results are grouped per file, in rank order: file name, folder name and relative
  path, line range, snippet with the query terms in bold. No scores in Basic (scores under Details). An empty result
  with `no_good_match` shows "Nothing closely matched “{query}” in your folders." With `no_folders` it shows the empty
  state "Add a folder to search it." Opening a file needs an opener capability that doesn't exist yet (OQ-40).
- **Source:** 02 §2.3 (hits: path, line range, snippet, scores); engine `SearchOutput` (`no_good_match`,
  `no_folders`); m2-r §3.

### D-37 [New] Answers with citations (M3 engine, M4 page)
- **Decision:** when a provider exists, the Search page offers "Ask" next to "Search". An answer shows its text with
  numbered citations [1] [2]. Each citation links to its result row (file, lines). Deeper answers are off by default
  ("Deeper answers (uses more AI credits, slower)").
- **Source:** 02 §2.3 answer (structured JSON), §2.5 Answers fields; 03 synthesizer row; U26. OQ-42.

### D-38 [New] Connected apps page (M4)
- **Decision:** one page lists the AI apps:
  - Claude Desktop (state per DS-5.5);
  - Cursor and VS Code ([Connect] writers, M4);
  - "Other app" (setup text block for stdio);
  - Advanced: "Local tools over HTTP" (off; when on, a per-client token snippet); per-app folder access ("All my
    folders" by default); "Let connected apps ask to add folders" with its pending requests.
- **Source:** 04 M4 row; 00 U18, Q3; 02 §2.5 Connected apps / Advanced integrations. OQ-44/45/46.

### D-39 [New] Activity page (M4)
- **Decision:** Activity is a read-only log with two sections:
  - "Recent activity": folders added or removed, runs finished, files skipped, search model changes, updates;
  - "Sent to AI providers": time, provider, purpose in plain words, size; from `egress_audit`, shown only once a
    provider was ever used.

  It is not needed for any task.
- **Source:** 04 M4 row "Activity"; 02 §2.4 `egress_audit`, `job_history`. OQ-47.

### D-40 [New] Collections are "Folder groups", Advanced only (M4)
- **Source:** 00 U15; 02 §2.4 collections; `config/folder.rs` CollectionSettings. OQ-48.

### D-41 [New] Managed settings (M5)
- **Decision:** a control set by `policy.toml` is shown disabled with a lock and "Managed by your organization"
  (DS-4.19). There is no separate page.
- **Source:** 02 §2.5 "policy.toml (M5) overrides with 'Managed by your organization'". OQ-50.

### D-42 [New] Import from Selene (M6)
- **Decision:** the import offer appears inside Selene (04 M6: "Selene shows 'Import my synced folders'"). Our app
  shows the imported folders as normal folder cards, with "Imported from Selene" in their detail. Page 34 specifies
  both sides.
- **Source:** 04 M6 row; 03 provenance rows. OQ-51.

### D-43 [New] Folder detail is where per-folder settings and skipped files live (M4)
- **Decision:** the target of "View skipped files" (code 9/12/13) is the folder detail's "Skipped files" section:
  file, reason in plain words, and a Retry where the code is retryable. Basic per-folder fields: name, keep up to date,
  file types, include online-only (M5). Advanced per-folder fields: files-only mode, reindex policy, include/exclude
  patterns, size limit, subfolders, check interval. (2026-11-08: files-only mode removed, Amendment 17.)
- **Source:** 02 §2.4 folders columns, §2.5 Folder (Basic) and Advanced indexing; `config/folder.rs`; `dto.rs`
  comment ("skipped-file screens arrive in M3/M4").

### D-44 [New] A stopped run reads "Stopped", not "Up to date"
- **Source:** m2-w §5.4; `state.rs` mapping. OQ-37.

### D-45 Minimal tray
- **Decision:** tray = icon (4 states) + tooltip "Selene Retrieval: {summary}" + menu: status line (disabled), Open
  Selene Retrieval, Quit. Left click opens the window [Code]. M4 adds a one-time "Still running next to the clock"
  notification on the first close (R-UX22).
- **Source:** `tray.rs`; 04 R-UX22. OQ-10, OQ-27, OQ-28.

### D-46 [New] Updates
- **Decision:** M2 keeps the manual "Check for updates" in Help. The endpoint is a placeholder, so the plain failure
  text "Couldn't check for updates" is expected (R-UX24). Installing shows "Updating · Selene Retrieval will restart
  when the update is ready." From M5 the app checks once at start and shows an in-app line "Version {v} is ready to
  install" with [Install] (OQ-29).
- **Source:** `Settings.tsx`, `updater.rs`; R-UX24; 02 §2.2 lifecycle.

### D-47 Window close and first close
- **Decision:** close = hide to tray [Code]. Quit only from the tray. M4: on the first close, a notification "Selene
  Retrieval is still running next to the clock. It keeps your folders up to date." (R-UX22 wording "Still running next
  to the clock").
- **Source:** m2-a §2; 04 R-UX22.
