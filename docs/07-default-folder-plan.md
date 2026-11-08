# 07. "Create a folder for me": a default folder in Documents

> Status: **Plan, approved by owner 2026-10-19 (orchestrated; proceed).**
> Binding: Product Principles P1 (average-user simplicity, never weakening security) and P2 (standing check), see `product-principles.md`. Amendments in `00-review-and-decisions.md` stay binding; this plan is recorded as **Amendment 13**.

Owner idea (2026-10-19): today the first thing a new user does is choose a folder to index. Instead, the first step offers two choices: **create a folder** or **add a folder**. "Create a folder" is one click. It makes a folder in Documents, opens it for the user, shows that folder's details screen, and the folder is ready: whatever the user puts there becomes searchable.

## 1. Findings in the current code

| # | Where | What it means for this feature |
|---|---|---|
| F1 | `ui/src/pages/Wizard.tsx:121-134` (`choose`), `:168-190` (step 1) | Step 1 has one action: [Choose a folder…] → Windows picker → `estimate_folder` → `add_folder`. Steps: pick/counting/big = 1, adding/progress = 2, provider = 3, connect = 4 (`:29-38`). Finish goes to `/` or `/providers` (`:136-148`). |
| F2 | `ui/src/pages/Overview.tsx:60` | Zero folders redirect to `/welcome` (the wizard). As soon as one folder exists the wizard is no longer forced, so a user who closes the window after step 2 lands on Overview, never stranded. |
| F3 | `ui/src/pages/Folders.tsx:147-165` (empty state), `:125` (header button) | "No folders yet" + [Add a folder]; the header has [Add a folder] (outline). |
| F4 | `crates/app/src/commands.rs:92-100`, `:678-690` | `add_folder` takes an absolute path from the UI and calls `folders.add`; the reply is merged into the pushed view (`state.rs` `apply_folder_info`). |
| F5 | `crates/common/src/api.rs:128-153` (`PRIVILEGED`, `folders.add` at `:140`) and `crates/engine/src/pipe.rs:137-160` (`trusted_peer`) | `folders.add` is already privileged (R31 / S-05): only our own `sr-app.exe` / admin CLI next to the daemon may call it. |
| F6 | `crates/engine/src/handle.rs:652` → `sr_sync` reconciler | The service validates every added root: canonical path, dangerous roots (`crates/common/src/sandbox/roots.rs:78-115`), allowed roots from policy (`:143`), overlap with added folders, network probe (`crates/sync/src/netprobe.rs:24`, 5 s). Errors: `PathRefused`, `FolderOverlap`, `FolderUnavailable`, `ManagedByPolicy`. |
| F7 | `crates/common/src/config/policy.rs:49-50, 495-507` | Policy can only *restrict* folders: `folders.allowed_roots` (empty = "No folders can be added."). There is no other "may add folders" key. The app sees locks as `AppView.policy.managed` (`crates/app/src/policy.rs:56-72`), with the roots only as a display string. |
| F8 | `crates/app/src/main.rs:194-207` (`client_roots`) | Precedent: per-user known folders come from Tauri's path resolver (`dirs` → `dirs-sys 0.5.0` `known_folder` = `SHGetKnownFolderPath(FOLDERID_*, 0)`), with a debug-only `dev_env` hook for tests. `document_dir()` is `FOLDERID_Documents` through the same code (`dirs-sys-0.5.0/src/lib.rs:150-194`). Already in `Cargo.lock`; **no new dependency**. |
| F9 | `crates/app/src/opener.rs:69-98, 128-162` | The only safe opener: Explorer by absolute path (System32 parent), never a shell, argument checked so Explorer never silently falls back to Documents (M5-C bug). `show_in_folder` only for files inside added folders, re-checked after canonicalisation. No "open folder" yet. |
| F10 | `crates/client/src/launch.rs:274` | The service runs from a per-user scheduled task with `<RunLevel>LeastPrivilege</RunLevel>`; the app manifest is `asInvoker` (`crates/app/build.rs`, M5-I). Neither is elevated in normal use. The extractor sandbox (`sr-extract`) has no file-system write access at all. |
| F11 | `crates/common/src/api.rs:164` (`ONLINE_ONLY_NOTE`), `crates/sync/src/fsid.rs:32-41` | OneDrive online-only files are skipped with the plain note "make them Always keep on this device"; per-folder `include_online_only` exists. |
| F12 | `installer/sr-setup/src/main.rs:371-394` (`delete_data`), `:459-540` (`uninstall`); `installer/nsis/selene-retrieval.nsi:222, 298-318`; `installer/wix/selene-retrieval.wxs:87` | Uninstall deletes only: the scheduled task, the data dir **when the user ticks** "Also delete my search index (your own files are never touched)", our client-config entries, the WebView cache under `--app-id`, `$INSTDIR`. Nothing under Documents is ever touched. |
| F13 | `crates/app/capabilities/default.json` | The WebView has no fs/shell/opener permission; app commands need none. A new command adds no capability. |
| F14 | `ui/tests/a11y/harness.mjs` | UI tests run the real bundle in jsdom with a fake IPC (`handlers`); a command without a fixture answers `null`. |

## 2. Decisions

| # | Topic | Decision | Why |
|---|---|---|---|
| D1 | Folder name | **`Selene Retrieval`** in Documents (the product name as in the Start menu). A *file* (or a link/junction) with that name → `Selene Retrieval (2)`, `(3)`, … up to `(20)` (Explorer's own numbering). | Plain and recognisable; the numbering is what Windows does for "New folder (2)". |
| D2 | Resolving Documents | `AppHandle::path().document_dir()` = `SHGetKnownFolderPath(FOLDERID_Documents)` (F8). Never `%USERPROFILE%\Documents`. Debug-only hook `SR_DOCUMENTS_DIR` (ignored in release, like every `dev_env`). | Follows OneDrive / folder redirection; no new crate; same pattern as `client_roots`. |
| D3 | Who creates the folder | **The desktop app** (`sr-app.exe`, the user's own non-elevated token), **not the service**. If the app runs elevated (user chose "Run as administrator"), it refuses with a plain message (an elevated folder would be owned by Administrators). | The background service stays **read-only towards the user's folders**: it never creates or writes anything there (it is reachable by MCP clients; keeping it write-free is the smaller attack surface). Windows ransomware protection (CFA) also only has to trust one program, the visible app. |
| D4 | RPC design | **No new RPC.** The app computes the path itself (the UI never sends one), creates the folder, then calls the **existing** privileged `folders.add` (R31, S-01 trusted-peer check) with it. The service validates it exactly like a user-picked folder (F6). New Tauri commands take no path: `get_default_folder()`, `create_default_folder()`, `open_folder(folder_id)` (the path comes from the app's own folder list). **R31 list unchanged.** | Smallest privileged surface: zero new pipe methods; the only new privileged effect ("make one fixed folder in Documents") lives in the UI process that already holds `folders.add`. |
| D5 | Policy | If the organisation manages `folders.allowed_roots` at all, the option is **hidden** (UI) and the command **refuses** with "Managed by your organisation" (fail closed; the app only has the roots as display text, F7). "Choose an existing folder" stays, with the existing roots hint. The service enforces the policy again on `folders.add`. | Fail closed; never create a folder the policy would refuse. |
| D6 | Wizard flow | Step 1 shows two choices. **[Create a folder for me]** (primary, recommended) → the folder is created, added and opened in Explorer → **step 2 becomes "Your folder is ready"** (folder card with live progress, the sentence "Anything you put in “Selene Retrieval” becomes searchable automatically.", [Next] focused, [Open folder]) → step 3 provider (optional) → step 4 connect apps → **Finish lands on that folder's details screen** (which repeats the ready line and has [Open folder]). | Same number of clicks as today minus the picker. The wizard keeps the provider and "Start with Windows" decisions (skipping them would leave autostart undecided, so the folder wouldn't stay up to date after a restart). Never stranded: the folder exists from click 1; closing the window anywhere lands on Overview (F2). |
| D7 | Outside the wizard | Folders page: the empty state offers [Create a folder for me] (primary) + [Choose an existing folder…] (outline); the header gets a ghost [Create a folder for me] next to [Add a folder] **only while the default folder isn't added** (nor covered by an added parent such as Documents). Success → that folder's details screen + toast. | Asked by the owner; hidden once done so it never nags. |
| D8 | Folder details | Every folder gets **[Open folder]** (opens it in Explorer; nothing is run). The default folder also shows "Anything you put here becomes searchable automatically." | "Open folder lets them get back to it." Opening a folder can't start a program, so it's safe for every added folder (unlike opening files, R-UX41). |
| D9 | Idempotent | Already added (or covered by an added parent) → nothing is created or added; it is opened and its details shown. Exists but not added → added as is (never wiped or renamed). Two clicks at once → serialised in the app. If `folders.add` fails right after *we* created the folder, the folder is removed again **only if still empty** (`remove_dir`, never recursive). | Requirement; never lose user data. |
| D10 | OneDrive | Documents in OneDrive → the folder is created there as a normal local folder (OneDrive syncs it like any folder). Files the user puts there are local; if OneDrive later frees them up ("online-only"), they are skipped with the existing note and [Include them] (F11). The ready screen says once "This folder is in OneDrive. Files kept only online aren't searched." We never change OneDrive pinning ourselves. | Existing behaviour; no surprise. |
| D11 | Network Documents | The create step runs off the UI thread with a **5 s limit** (same as the service's network probe, F6). No answer → "Your Documents folder isn't responding" + [Choose a folder]. `folders.add` applies the existing 5 s probe; the folder then shows the "Network drive" note. | Never hang the UI on a dead share. |
| D12 | Uninstall | Unchanged: uninstall never touches Documents (F12). The folder and its contents are the user's data. Recorded in the VM-matrix round trip. | Requirement. |
| D13 | Network / telemetry | None. Creating the folder makes no connection; first run still makes 0 network connections. | P1. |

## 3. UX flow (text wireframes)

### 3.1 Wizard step 1 (the offer is available)

```
● ○ ○ ○  Step 1 of 4
┌──────────────────────────────────────────────────────────────┐
│ Choose a folder to search                                  (h1) │
│ Selene Retrieval reads a folder on this computer and keeps it   │
│ up to date. Your files are not uploaded anywhere.               │
│                                                                  │
│ [ 📁+ Create a folder for me ]            (primary, lg)          │
│   Makes a “Selene Retrieval” folder in Documents. Anything you  │
│   put there becomes searchable.            (aria-describedby)   │
│                                                                  │
│ [ 🔍 Choose an existing folder… ]          (outline)              │
└──────────────────────────────────────────────────────────────┘
```
While creating: the primary button shows the spinner (busy) and the status line "Creating your folder…" (`role=status`). On failure: the ErrorPanel (one action, always [Choose a folder] → the picker) above the buttons; both buttons stay.
Offer not available (policy, Documents not found): only [Choose a folder…] (primary), exactly as today.

### 3.2 Wizard step 2 after "Create a folder for me"

```
● ● ○ ○  Step 2 of 4
┌──────────────────────────────────────────────────────────────┐
│ Your folder is ready                                       (h1) │
│ Anything you put in “Selene Retrieval” becomes searchable      │
│ automatically. It's in your Documents folder.                   │
│ We opened it in File Explorer for you.      (only if opened)     │
│ ☁ This folder is in OneDrive. Files kept only online aren't     │
│   searched.                                 (only in OneDrive)   │
│ ┌ folder card (live state: "Up to date · 0 files") ┐            │
│ You can keep working. Indexing continues in the background,    │
│ even if you close this window.                                  │
│ [ Next ] (primary, focused)   [ Open folder ] (outline)         │
└──────────────────────────────────────────────────────────────┘
```
Steps 3 and 4 are unchanged. [Finish] then opens `/folders/<id>`.

### 3.3 Folder details (page 25), header card additions

```
Selene Retrieval                                            (h1)
C:\Users\Ana\Documents\Selene Retrieval  [Copy path] [Open folder]
Up to date · 3 files
Anything you put here becomes searchable automatically.  (default folder only)
```

### 3.4 Folders page

Empty:
```
   📁  No folders yet                                         (h2)
   Create a folder in Documents for your files, or choose one you already have.
   [ Create a folder for me ] (primary)   [ Choose an existing folder… ] (outline)
```
With folders (offer available): header `[ Create a folder for me ] (ghost sm)  [ + Add a folder ] (outline sm)`.

### 3.5 Errors (each: title, body, one action; the action is always the fallback "Choose a folder")

| Case | Title | Body |
|---|---|---|
| Access denied (permissions or Windows ransomware protection, CFA) | Couldn't create the folder | Windows didn't let Selene Retrieval create a folder in Documents. If Windows Security showed “Unauthorized changes blocked”, its ransomware protection stopped it. Choose a folder you already have instead. |
| Disk full | Not enough disk space | There isn't enough space to create a folder in Documents. Free up some space, or choose a folder you already have. |
| Documents not found | Your Documents folder can't be found | Choose a folder you already have instead. |
| No answer in 5 s (network Documents) | Your Documents folder isn't responding | It may be on a network drive that can't be reached right now. Choose a folder you already have instead. |
| App running as administrator | Selene Retrieval is running as administrator | Folders it makes now would belong to the administrator account. Open Selene Retrieval normally, or choose a folder you already have. |
| Policy | Managed by your organisation (service renderer text) | + the existing allowed-roots hint. |
| Anything else from `folders.add` | the service's own UserMessage | action replaced by [Choose a folder] when it had none. |

Accessibility: the buttons have visible text names; the hint is the primary button's `aria-describedby`; step changes move focus as today (step 2 focuses [Next]); errors are `role=alert` (ErrorPanel). Rows use `flex-wrap`; no fixed widths, so 100–200 % scaling never clips.

## 4. API

Rust (`crates/app`):
- `sr_client::default_folder` (new, std only, shared by app and tests): `FOLDER_NAME`, `candidates(docs)`, `is_default_folder(docs, path)`, `covering_folder(docs, added)`, `make(docs) -> Result<Made{path, created}, MakeError>` (no `create_dir_all`; Documents must exist; reparse points count as taken; `AlreadyExists` races retried), `MakeError` classified from the OS error (`AccessDenied`/`DiskFull`/`DocumentsMissing`/`Other`).
- `sr_client::win::process_elevated()` (`GetTokenInformation(TokenElevation)`, reviewed FFI module).
- `crates/app/src/default_folder.rs`: `DefaultFolderOffer {available, folder_id, name, path}`, `DefaultFolderResult {folder_id, name, path, created, already_added, opened, in_onedrive}`, `offer(...)`, `async create<R: Rpc>(rpc, docs, added, roots_managed, elevated, timeout)`, the error texts (§3.5).
- `opener::open_folder(path, roots)`: an existing directory that is (or is inside) an added folder, re-checked after canonicalisation, ≤ 259 chars, quoted; Explorer by absolute path.
- Tauri commands (no path parameter): `get_default_folder() -> DefaultFolderOffer`, `create_default_folder() -> Result<DefaultFolderResult, UiError>` (serialised by a mutex; opens Explorer after success), `open_folder(folder_id) -> Result<(), UiError>`.
- Service: **unchanged** (`folders.add`).

UI: `ui/src/lib/defaultFolder.ts` (`useDefaultFolder()` re-reads only when the pushed folder list or policy changes; never on a timer), `Wizard.tsx`, `Folders.tsx`, `FolderDetail.tsx`; `bindings.ts` regenerated.

## 5. Files to touch

- `crates/client/src/default_folder.rs` (new), `crates/client/src/lib.rs`, `crates/client/src/win.rs` (`process_elevated`).
- `crates/app/src/default_folder.rs` (new), `crates/app/src/main.rs` (module + command registration + `documents_dir`), `crates/app/src/commands.rs` (three commands), `crates/app/src/opener.rs` (`open_folder`).
- `crates/engine/tests/default_folder.rs` (new; engine/RPC flow).
- `ui/src/bindings.ts`, `ui/src/lib/defaultFolder.ts` (new), `ui/src/pages/Wizard.tsx`, `ui/src/pages/Folders.tsx`, `ui/src/pages/FolderDetail.tsx`, `ui/tests/default-folder.test.mjs` (new), wizard label in `ui/tests/a11y.test.mjs` / `claude-signin.test.mjs` only if the old label changes (it doesn't when the offer is absent).
- Docs: this file, `00-review-and-decisions.md` (Amendment 13), `04-roadmap.md` (risks), `02-architecture.md` §2.6 (one line: the default folder is still added through the app UI; the service never writes into user folders).

Not touched: engine RPC dispatch, `PRIVILEGED`, `sr-sync`, `sr-core`, `sr-catalog`, MCP tools (the output-efficiency phase owns them), installers.

## 6. Edge cases

| Case | Behaviour |
|---|---|
| `Selene Retrieval` exists and is added | Nothing created or added; opened; details shown (`already_added`). |
| Exists, not added | Added as is; contents kept; indexed. |
| A file `Selene Retrieval` | `Selene Retrieval (2)` is used; the next click finds `(2)` already added. |
| A link / junction named `Selene Retrieval` | Treated as taken (never follow a link the user didn't pick) → `(2)`. |
| `(2)` … `(20)` all taken by files | "Couldn't create the folder" → [Choose a folder]. |
| Documents (or a parent) already added | Offer hidden. Via a race: the folder is created inside it, opened, and the parent's details shown (it's already searchable). |
| The default folder was added, then deleted in Explorer | The existing "Folder not available" state; offer stays hidden (it's still added). Removing it in the app brings the offer back; a new click recreates it. |
| Removed from the app, folder still on disk | The next click re-adds the existing folder (never wiped). |
| Double click / two windows | Serialised; the second click sees it added. |
| `folders.add` fails after we created it | The empty folder is removed again (non-recursive); the service's message is shown with [Choose a folder]. |
| Policy `allowed_roots` managed (incl. empty, or an unreadable policy file = fail closed) | Offer hidden; command refuses `ManagedByPolicy`. |
| CFA on | `ERROR_ACCESS_DENIED` → the CFA-aware message; the user can choose a folder they made in Explorer (Explorer is trusted by CFA). |
| Disk full | `ERROR_DISK_FULL` / `ERROR_HANDLE_DISK_FULL` → "Not enough disk space". |
| Documents on an unreachable share | 5 s limit → "isn't responding". |
| Path longer than Explorer can open (> 259) | Created and added; Explorer isn't started (`opened = false`); [Open folder] reports "Couldn't open it". |
| File dropped in within ~2 s of the add (before the live watcher armed the new folder, R45) | The app asks the service for one catch-up check of that folder 3 s after the add, so the file is found without waiting for the hourly full check. |
| Service not running | `folders.add` fails with the existing `ServiceUnavailable` panel; the empty folder is removed. |
| App elevated | Refused before touching the disk (§3.5). |
| Older service | Nothing new is needed from it. |

## 7. Test plan

Rust (unit, `sr-client`): candidates and numbering; idempotent `make` (second call `created=false`); file collision → `(2)`; a junction is taken; Documents missing → `DocumentsMissing`; `is_default_folder` / `covering_folder` (case-insensitive, `\\?\`, trailing separators, a sibling `Selene Retrieval Old` isn't matched).
Rust (unit, `sr-app`, `FakeRpc` + injectable temp Documents): create-and-add sends exactly one `folders.add` with the computed path; already added → no call; covered by Documents → no call, parent id; policy managed → refusal before any disk change; elevated → refusal; `folders.add` failure removes the empty folder but never a non-empty one; error texts have one action (`choose_folder`); `offer` hidden/visible; `open_folder` stays inside added folders.
Engine/RPC (`crates/engine/tests/default_folder.rs`): `make` in a temp Documents → `folders.add` over the RPC dispatcher → indexed; a file dropped in becomes searchable through the live watcher; a second `make` is idempotent and a second `folders.add` is refused (`FolderOverlap`), so a duplicate can never be created.
UI (`ui/tests/default-folder.test.mjs`): the wizard shows both choices with names and the description; one click calls `create_default_folder` (never `add_folder` or the picker) and shows "Your folder is ready" with focus on [Next], [Open folder] calls `open_folder`, Finish goes to the folder's details; failure shows the panel and [Choose a folder] opens the picker; policy hides the offer; Folders empty state offers both; header button only while not added; axe clean on each.
Live (after the gate): a debug daemon on its own `--data-dir` + a temp fake Documents; the app's `create` against it: created, added, indexed, watched, a dropped file searchable, second click idempotent. The owner's install and real Documents are never touched.

## 8. Standing check

"Can an average user do this without documentation?" **Yes.** One click with a recommended label; the folder opens on screen; the next screen says what to do in one sentence; [Open folder] is always on the folder's page. Failures say what happened in plain words and offer the one thing that always works (choose a folder). Security defaults are unchanged and invisible (same validation, same privileged method, policy fail-closed). New risks: R-UX49 (CFA), R-UX50 (OneDrive / name not localised) and R45 (watcher arms a new folder up to 2 s after the add; mitigated for this flow by one catch-up check), logged in `04-roadmap.md`.

## 9. Results (2026-10-20)

- **Gate** (consolidated, `-j 12`, also covers the output-efficiency phase): g1 green except `fmt` (one file of this change); one fix batch; g2 all green: workspace tests 817 pass / 0 fail / 7 ignored, clippy `-D warnings`, fmt, `cargo deny` ×3, sr-setup 40/40, msi-ca 3/3, check-bridge-deps / check-crt / check-delayload / check-manifest / `sync-providers --check` OK, UI ci / typecheck / lint / 61 tests / build OK, bindings test OK.
- **Flake fixed before g1:** the first engine test dropped a file into the new, empty folder right after the add; the watcher arms a new folder up to 2 s later, so the file was missed 2 of 3 runs (R45). The test now covers the app's catch-up check and then a later file through the watcher alone: 10/10 runs green.
- **Installers** (dist, unsigned): NSIS 124.1 MB, MSI, and both offline variants; check-manifest / check-crt / check-delayload OK on the shipped exes.
- **Live check:** a debug daemon on its own `--data-dir` and a temp Documents folder, driven by the app's real `create` path over the pipe: the folder was created and added, became idle (indexed), the watcher armed it, a file dropped in at once was searchable after the catch-up check, a later file through the watcher alone (0.8 s), and the second click was idempotent (`already_added`, still one folder). The owner's install and real Documents folder weren't touched.
