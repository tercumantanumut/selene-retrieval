# Selene Retrieval design system

Status: **FINAL** (baseline = the M2 code; changes are marked **Change** and listed as open questions).
Binding: `docs/product-principles.md` P1/P2, `docs/00-review-and-decisions.md` Amendments 1–5, `docs/02-architecture.md` §2.5.

The baseline is the implemented UI: `ui/src/index.css` (tokens), `ui/src/components/ui/*` (Button, Card, Progress, Switch,
Collapsible), `ui/src/components/*` (StatusPill, FolderCard, ErrorPanel, ApprovalCard, ClaudeConnect), sonner toasts,
lucide icons, Tailwind 4. Every page brief cites the sections of this file by number (DS-n).

Conventions used in every brief:
- **[Code]**: exists in `ui/src` or `crates/app` today. Copy marked [Code] must be used verbatim.
- **[Doc]**: decided in `docs/` but not yet in the code.
- **[New]**: designed here, because the sources specify the feature but not its UI. Each one has a decision record
  in `00-design-decisions.md` and, where a real choice remains, an open question (OQ-n).
- **Assumption:** a fact that couldn't be verified from the sources. It is never presented as decided.

---

## DS-1 Tokens

### DS-1.1 Colour

The tokens live in `ui/src/index.css` as Tailwind 4 `@theme` variables. Dark values apply through
`@media (prefers-color-scheme: dark)`, so the app follows the Windows light/dark setting with no in-app switch.
Hex values are the sRGB conversions of the OKLCH source values. Contrast ratios were computed with the WCAG 2.x
relative-luminance formula. Ratios on tinted panels (`/10` mixes) are approximate, because Tailwind mixes in OKLab.
Re-check them with a contrast tool once the change is in.

| Token | Light (OKLCH → hex) | Dark (OKLCH → hex) | Use |
|---|---|---|---|
| `background` | 0.99 0 0 → `#FCFCFC` | 0.18 0.015 265 → `#0E1218` | Window, cards, outline buttons |
| `foreground` | 0.2 0.02 265 → `#12161F` | 0.95 0.005 265 → `#EDEEF2` | Body text, titles |
| `muted` | 0.96 0.005 265 → `#F0F2F5` | 0.24 0.015 265 → `#1C1F27` | Progress track, code snippet background |
| `muted-foreground` | 0.5 0.02 265 → `#5E636F` | 0.72 0.015 265 → `#A0A5AE` | Secondary text, paths, help lines |
| `border` | 0.91 0.01 265 → `#DEE1E8` | 0.3 0.015 265 → `#2A2E36` | Card and header borders (decorative only) |
| `primary` | 0.33 0.07 265 → `#233459` | 0.82 0.06 265 → `#B1C4EC` | Primary buttons, progress fill, links, active spinner |
| `primary-foreground` | 0.98 0 0 → `#F8F8F8` | 0.2 0.02 265 → `#12161F` | Text on primary |
| `accent` | 0.95 0.01 265 → `#EBEEF5` | 0.26 0.02 265 → `#1F242E` | Hover fill, active nav item |
| `success` | 0.6 0.15 150 → `#25984D` | same (not redefined) | "Up to date" icon and dot |
| `warning` | 0.7 0.16 65 → `#E18600` | same (not redefined) | Attention icon, ErrorPanel border and tint |
| `danger` | 0.58 0.2 27 → `#D73431` | same (not redefined) | Destructive button (white text) |

Measured contrast (WCAG 2.2 AA: text 4.5:1, large text and non-text UI 3:1):

| Pair | Light | Dark | Verdict |
|---|---|---|---|
| foreground / background | 17.60 | 16.26 | Pass |
| muted-foreground / background | 5.83 | 7.58 | Pass (text) |
| muted-foreground / muted | 5.34 | 6.63 | Pass |
| primary-foreground / primary | 11.62 | 10.35 | Pass |
| primary (link text, focus ring) / background | 11.96 | 10.75 | Pass |
| foreground / accent (hover, active nav) | 15.65 | 13.44 | Pass |
| white / danger (danger button) | 4.73 | 4.73 | Pass (text) |
| success icon / background | 3.59 | 5.09 | Pass (non-text) |
| **warning icon / background** | **2.69** | 6.78 | **Light fails 3:1** |
| danger as text / background | 4.60 | **3.98** | Dark fails 4.5:1 as text (no text uses it today) |
| **border as a control edge** (Switch "off" track = `bg-border`) | **1.27** | **1.38** | **Fails 3:1** (WCAG 1.4.11) |
| muted-foreground on ErrorPanel tint (`warning/10`) | ≈ 5.47 | ≈ 4.80 | Pass |

**Change (OQ-11): three tokens are added. No existing value changes.**

| New token | Light | Dark | Replaces in |
|---|---|---|---|
| `warning-strong` | 0.52 0.13 60 → `#9D5300` (5.56 on bg) | 0.78 0.15 70 → `#F4A437` (9.15) | Warning **icons** and the ErrorPanel icon. The `warning` tint and border stay. |
| `control-off` | 0.6 0.01 265 → `#7D8086` (3.84) | 0.55 0.015 265 → `#6D727B` (3.88) | Switch "off" track, checkbox and radio borders, text-input borders |
| `danger` (dark only) | — | 0.68 0.19 27 → `#F75E54` (5.99) | Danger text and icons in dark mode. The danger **button** keeps 0.58 0.2 27 with white text (4.73). |

With `warning-strong` (dark), muted-foreground on the ErrorPanel tint drops to about 4.18. ErrorPanel body text
therefore uses `foreground` at full strength, never `muted-foreground` (DS-4.7).

Rules:
- Colour never carries meaning alone. Every state also has an icon and a text label (DS-5).
- `success` is used only for "done/connected" icons and dots, never for body text.
- `danger` is used only for destructive actions (remove a folder, delete data), never for errors. Errors use the
  warning style (00 D-11).

### DS-1.2 Typography

Font stack [Code]: `"Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif`. No web fonts are shipped. The CSP
allows `font-src 'self'` and nothing is bundled. Antialiasing: `-webkit-font-smoothing: antialiased`.

Type scale (Tailwind 4 defaults, as used):

| Role | Class | Size / line height | Weight | Where |
|---|---|---|---|---|
| Wizard step title | `text-xl` | 20 / 28 px | 600 (`CardTitle`) | Wizard cards |
| Page title | `text-lg font-semibold` | 18 / 28 px | 600 | "Your folders", "Settings" |
| Card title | `text-base font-semibold leading-tight` | 16 / 20 px | 600 | `CardTitle` |
| Large button | `text-base` | 16 / 24 px | 500 | `Button size=lg` |
| Body / button | `text-sm` | 14 / 20 px | 400 / 500 | Default text, buttons, folder line |
| Small / path / Details | `text-xs` | 12 / 16 px | 400 | Folder path, "Details" text |
| Code | `code` (monospace system font) | 12–14 px | 400 | Index location, setup text, Details |
| Brand in header | `font-semibold tracking-tight` | 16 px | 600 | "Selene Retrieval" |

Rules:
- Text sizes are in `rem`, so Windows "Text size" (Accessibility → Text size) scales them. Layouts must hold at 200 %
  (DS-6).
- Sentence case everywhere ("Check for changes now", not "Check For Changes Now").
- Titles carry no final full stop (the MCP renderer adds one; 02 §2.5).
- Folder and file names are quoted with typographic quotes: “corpus” [Code `Wizard.tsx`]. The error renderer uses
  straight quotes `"…"` [Code `messages.rs`]. **Assumption:** the mix is accepted for M2. OQ-11 covers unifying it in
  the renderer when localisation lands.

### DS-1.3 Spacing

Base unit 4 px (Tailwind 4 `--spacing: 0.25rem`). Values in use [Code]:

| Token | px | Use |
|---|---|---|
| `1` | 4 | Nav item gap |
| `1.5` | 6 | Icon-to-text in a folder title (`mr-1.5`) |
| `2` | 8 | Icon-to-text in rows, gap between small buttons, `space-y-2` stacks |
| `3` | 12 | Gap between cards (`gap-3`), header vertical padding, card internal stacks |
| `4` | 16 | Panel padding (ErrorPanel, ApprovalCard, Collapsible), page section gap (`space-y-4`) |
| `5` | 20 | Card padding (`p-5`) |
| `6` | 24 | Page gutters (`px-6`), page top/bottom (`py-6`), section gap (`space-y-6`) |
| `10` | 40 | Centered loading line (`py-10`) |

### DS-1.4 Radius

| Token | Value | Use |
|---|---|---|
| `rounded-md` | 6 px | Buttons, nav items, inputs |
| `rounded-lg` (`--radius-lg`) | 12 px | Cards, panels, Collapsible, dialogs, snippet blocks |
| `rounded-full` | 9999 px | StatusPill, status dots, switch, progress track |

### DS-1.5 Elevation

| Level | Token | Use |
|---|---|---|
| 0 | none | Page, header, panels (ErrorPanel, ApprovalCard) |
| 1 | `shadow-xs` | Cards [Code] |
| 2 | `shadow` | Switch thumb [Code] |
| 3 | `shadow-lg` + `bg-black/40` backdrop | Dialogs [New] |
| Toast | sonner default | Toasts, bottom-right [Code] |

### DS-1.6 Motion

| Motion | Duration / easing | Where | Reduced motion (`prefers-reduced-motion: reduce`) |
|---|---|---|---|
| Colour transitions | 150 ms, `cubic-bezier(0.4,0,0.2,1)` (Tailwind default) | Button, switch hover/state | Kept (no movement) |
| Progress fill width | 300 ms (`duration-300`) | Determinate Progress | Off: width changes instantly |
| Switch thumb | 150 ms translate | Switch | Off |
| Chevron rotate | 150 ms | Advanced disclosure | Off |
| Spinner | `animate-spin` 1 s linear, infinite | Loader2 in folder state, "Starting…" | Replaced by a static `Loader` icon. The text label already carries the state. |
| Pulse | `animate-pulse` 2 s, infinite | Status dot (starting, indexing), indeterminate Progress | Off: a static dot; the indeterminate bar shows a static 33 % segment at the left |
| Toast enter/exit | sonner default | Toasts | sonner honours reduced motion. **Assumption:** verify in WebView2 |

**Change (OQ-11):** today no `motion-safe:`/`motion-reduce:` variants exist in `ui/src`. Every infinite animation
gets a `motion-reduce:` override as listed. Nothing animates for longer than 5 s without a text equivalent
(WCAG 2.2.2).

---

## DS-2 Layout and window

| Item | Value | Source |
|---|---|---|
| Default window | 920 × 700 px, centered, starts hidden and is shown once ready | [Code] `tauri.conf.json` |
| Minimum window | 640 × 540 px | [Code] |
| Close button | Hides the window to the tray. Quit only from the tray menu. | [Code] `main.rs` |
| Header | Full width, `border-b`, `px-6 py-3`: brand (left), nav (left, after the brand, hidden in the wizard), StatusPill (right, `ml-auto`, truncates). About 55 px tall. | [Code] `router.tsx` |
| Main column | `max-w-3xl` (768 px), centered, `px-6 py-6`, `space-y-4` | [Code] |
| Wizard column | `max-w-xl` (576 px) inside the main column, `py-6`, `space-y-6` | [Code] |
| Global stack (top of main, in order) | 1 service problem (ErrorPanel) → 2 notices (ErrorPanel + Dismiss) → 3 approval cards (not in the wizard) → page outlet | [Code] |
| Toasts | Bottom-right, rich colours, close button | [Code] |
| Grid | Single column. Cards stack with `gap-3`; sections with `space-y-6`. No multi-column layouts below 900 px of content. | [Code] |
| Navigation (M4) | [New] Header nav grows to the M4 page set. At window widths below 760 px the nav collapses into a "Menu" button (DS-4.17). | 04 M4 row |
| Tray | Native Windows tray icon plus a native context menu. **No popover window.** | [Code] `tray.rs` |
| Scroll | The page scrolls under a static header. **Change (OQ-11):** make the header sticky (`sticky top-0 bg-background z-10`), so the status stays visible on long pages such as Settings (see screenshot 07). | [New] |

Behaviour at the minimum size (640 px): the content column is 592 px. Card action rows wrap (`flex-wrap`). The
StatusPill truncates with an ellipsis and keeps the full summary in its tooltip (`title`).

---

## DS-3 Iconography

Library: **lucide-react** [Code], 1.5–2 px stroke, `currentColor`. Sizes: 16 px (`size-4`, inline and in buttons, set
by the Button `[&_svg]:size-4` rule) and 20 px (`size-5`, state icons). Decorative icons carry `aria-hidden`
[Code]; meaning always comes from adjacent text.

| Meaning | Icon | Colour |
|---|---|---|
| Up to date / connected | `CheckCircle2` | success |
| Working (waiting, scanning, indexing, starting) | `Loader2` spinning | primary |
| Needs attention / error | `AlertTriangle` | warning-strong (DS-1.1 change) |
| Paused | `PauseCircle` | muted-foreground |
| Folder | `FolderOpen` (inline before the name) | muted-foreground |
| Add folder / folder request | `FolderPlus` | primary (ApprovalCard), inherit (button) |
| Choose a folder | `FolderSearch` | inherit |
| Connect | `Plug` | inherit |
| Copy | `ClipboardCopy` | inherit |
| Disclosure | `ChevronRight` (rotates 90° open) | inherit |
| [New] Network drive note | `Network` | muted-foreground |
| [New] Was set to only list its files (Amendment 17) | `ListTree` | muted-foreground |
| [New] Search | `Search` | inherit |
| [New] Answer / AI | `Sparkles` | inherit (only where a provider is used, M3+) |
| [New] Managed by your organization | `Lock` | muted-foreground |
| [New] Removing | `Trash2` + spinner | muted-foreground |
| [New] Download | `Download` | inherit |
| [New] External / data leaves the computer | `CloudUpload` | warning-strong (consent dialogs only) |

Tray icons [Code] `crates/app/icons/`: `tray-idle.png`, `tray-indexing.png`, `tray-synced.png`, `tray-problem.png`
(32 px). **Change (OQ-5):** add 16, 20 and 24 px variants for 100–150 % scaling (open item in m2-a-app.md §10), and
light-taskbar and dark-taskbar versions. The four shapes must differ in shape, not only colour (DS-5.2).

---

## DS-4 Core components

Each component lists variants, states, keyboard behaviour and its code location. Briefs reference them as
`DS-4.n Name`.

### DS-4.1 Button [Code `ui/button.tsx`]

| Variant | Look | Use |
|---|---|---|
| `default` | primary fill, primary-foreground text | The one main action of a card or step |
| `outline` | border, background; hover accent | Secondary actions, the action button in ErrorPanel |
| `ghost` | text only; hover accent | Low-emphasis (Pause, Remove…, Keep, Dismiss, Don't allow) |
| `link` | primary text, underline on hover, no horizontal padding | Inline alternative ("Copy setup text instead") |
| `danger` | danger fill, white text | Confirming a destructive action only |

Sizes: `sm` 32 px tall (`h-8 px-3`), `default` 36 px (`h-9 px-4`), `lg` 44 px (`h-11 px-6 text-base`).
All are at least 24 × 24 px (WCAG 2.5.8).

States: hover; focus-visible (2 px ring, `primary/50`); disabled (50 % opacity, no pointer events); busy (disabled
plus, **Change**, a leading `Loader2` and an unchanged label). Icon-only buttons need an `aria-label`. Today none
exist.

Rule: at most one `default` button per card or step.

### DS-4.2 Card [Code `ui/card.tsx`]
`rounded-lg border bg-background p-5 shadow-xs`. `CardTitle` is an `h2` (16 px, 600). `CardText` is 14 px,
muted-foreground. Cards group one topic: a folder, "Use it in Claude", a settings section.

### DS-4.3 Folder card / folder row [Code `FolderCard.tsx`]
Regions, top to bottom:
1. State icon (20 px, DS-5.1), then name (600, truncate, `title` = full name) with an inline `FolderOpen`.
2. Path (12 px, muted, truncate, `title` = full path). Hidden when unknown (`path: null`).
3. Status line (14 px, muted) = `FolderView.line` (DS-5.1).
4. [New] Note line (12 px, muted, with an icon): network drive (`Network`) or "was set to only list its files"
   (`ListTree`, Amendment 17). See DS-5.3.
5. Progress (DS-4.5) only while busy (`waiting`, `scanning`, `indexing`).
6. Action row (hidden when `compact`): "Check for changes now" (outline sm; not when busy or paused) · "Pause" (ghost)
   or "Resume" (outline; when paused; Pause is hidden when unavailable; reads "Search its contents" for a folder
   that was set to only list its files, Amendment 17) · "Remove…" (ghost) → inline confirm (DS-4.9).

Variants: `compact` (wizard step 2: no actions) [Code]; `row` [New, M4 Folders list]: one line, 56 px tall, with the
icon, name, status line and a chevron. The whole row is a link to the folder detail.

### DS-4.4 Status badge and StatusPill [Code `StatusPill.tsx`]
- **StatusPill** (header): rounded-full, border, 8 px dot (colour and animation per Overall, DS-5.2), plus text =
  `AppView.summary` (falls back to the state label), truncated, `title` = summary.
- **Status badge** [New, M4 lists and folder detail]: the same shape, 12 px text, no border, tinted `muted`
  background, with the DS-5.1 icon at 14 px instead of the dot. The label is the short label from DS-5.1.

### DS-4.5 Progress [Code `ui/progress.tsx`]
- `role="progressbar"`, `aria-valuemin=0`, `aria-valuemax=100`, `aria-valuenow` when determinate.
- Determinate: fill width = value % (minimum 2 %, so 0 % is visible), 300 ms transition.
- Indeterminate (`value=null`): a pulsing 33 % segment.
- Height 8 px, track `muted`, fill `primary`.
- **Change:** every Progress gets an accessible name (`aria-label`, or `aria-labelledby` pointing at its status line)
  and `aria-valuetext` (e.g. "Indexing 340 of 2,100 files"). Screen readers otherwise announce a bare percentage.

### DS-4.6 Toast [Code sonner, `lib/app.ts`]
- Types: success (e.g. "Folder added", "Setup text copied…"), info ("Saved", "Updating", "Restart Claude", "Free up
  disk space"), error (from `useErrorToast`: title, body as description, at most one action button with
  `UiError.action_label`).
- Position bottom-right, close button, default sonner timeout.
- Rules: a toast never holds the only copy of something the user must act on. Problems that persist also appear as
  an ErrorPanel or folder state. Toasts are polite live regions (sonner default).

### DS-4.7 Error panel with Details [Code `ErrorPanel.tsx`] = the notice banner
- `role="alert"`, `rounded-lg border-warning/40 bg-warning/10 p-4`, `AlertTriangle` icon.
- Content: title (500) → body (14 px; **Change:** `foreground` instead of `muted-foreground`, DS-1.1) → one action
  button (outline sm, label = `action_label`; omitted when `action.kind = none`) → optional "Dismiss" (ghost sm,
  notices only) → `<details>` "Details" (12 px), showing `UiError.details` = `"<Code> · trace <id>"` in `code`.
- The same component renders service problems, notices ("Some settings were reset"), inline wizard errors and
  model-change failures. **There is exactly one error shape** (P1.5, 02 §2.5).
- **Change:** use `role="alert"` only for panels that appear in response to a change. Notices present at page load
  use `role="status"`, so every navigation doesn't re-announce them.

### DS-4.8 Approval card [Code `ApprovalCard.tsx`]
`role="alert"`, `border-primary/40 bg-primary/5 p-4`, `FolderPlus` (primary). Shows "{client} wants to add a folder"
(500), the path (14 px, muted, break-all), and [Allow] (default sm) [Don't allow] (ghost sm). Both are disabled while
busy.

### DS-4.9 Dialog and confirm
- **Inline confirm** [Code `FolderCard`]: the action row is replaced by a sentence plus [Remove] (danger sm) and
  [Keep] (ghost sm). Use it for confirmations with one consequence.
- **Dialog** [New] (Radix Dialog / AlertDialog from the `radix-ui` package already in use):
  - centered, width 480 px (max `calc(100vw-48px)`), `rounded-lg`, `p-6`, `shadow-lg`, backdrop `bg-black/40`;
  - title (`text-lg` 600), body (14 px), optional checkbox or list, then a right-aligned footer: secondary (ghost or
    outline) first, primary (`default`, or `danger` for destructive actions) last;
  - focus goes to the **least destructive** button when the dialog opens;
  - Esc and backdrop click close it (cancel) except while an operation runs;
  - focus is trapped inside and returns to the trigger on close.
  - Use it for consent (data leaves the computer, M3), moving the index (M4) and anything with more than one
    consequence.

### DS-4.10 Empty state [New]
Centered in its card or page: a 32 px muted icon, a title (`text-base` 600), one sentence (14 px muted), and at most
one primary button. Examples are in each brief. The Overview never shows an empty state, because zero folders
redirects to the wizard [Code].

### DS-4.11 Toggle (Switch) [Code `ui/switch.tsx`]
- Radix Switch, 36 × 20 px, thumb 16 px.
- Checked = primary track. Unchecked = `border` track; **Change:** use `control-off` (DS-1.1).
- Always inside a `<label>` with the title and a muted help line; the whole row is the hit area.
- **Change:** set `aria-describedby` to the help line.
- Render it only once its real value is known, so it never flashes "off" [Code].

### DS-4.12 Select and radio cards (search quality tiers) [Code + New]
- Today [Code]: Advanced → "Search model" is a row of three buttons ("Basic search", "Better search", "Best search").
  The active one is `default` and disabled; the others are `outline` with a size tooltip ("built in",
  "105–208 MB download", "638 MB download").
- **Change (OQ-11, M4):** replace the row with a **radio-card group** (`role="radiogroup"`). Each card shows:
  - the label (Basic / Better / Best search);
  - one plain line: "Works on every computer. Built in." / "Better results. {105 or 208} MB download." / "Best
    results. 638 MB download.";
  - a "Recommended for your computer" tag on the recommended tier;
  - "In use" on the active tier.

  Arrow keys move between cards. Selecting a card starts the change at once; the click is the consent (Amendment 1).
  The warning line shows under the group when the choice is above the recommendation.
- **Select** [New] (Radix Select) for single choices with more than three options (provider list, file types preset,
  log level). It has a visible label, a 36 px trigger and opens with the keyboard.

### DS-4.13 Code snippet with Copy [Code `ClaudeConnect` + New block]
- Today [Code]: the snippet is never shown on screen. A "Copy setup text" button copies it, with the toast "Setup
  text copied. Paste it into your AI app's MCP settings, then restart that app."
- [New, M4 Connected apps → Other app]: a read-only block (`rounded-lg bg-muted p-3`, monospace 12 px, horizontal
  scroll, max 12 lines, `aria-label="Setup text"`) with a "Copy" button (outline sm, `ClipboardCopy`) above its top
  right. Tokens in HTTP snippets are masked on screen as `••••` + the last 4 characters, and copied in full (OQ-45).

### DS-4.14 Wizard stepper [Code + Change]
- Today [Code]: the text "Step {n} of 3" (14 px muted) above the step card. The step mapping is pick/counting/big = 1,
  adding/progress = 2, connect = 3.
- **Change (OQ-12):** add three 8 px dots to the left of the text (filled primary = done or current, `control-off`
  outline = to come), `aria-hidden`. The text stays the accessible label and becomes the `aria-current="step"` region
  label. From M3 (with the optional provider step, page 20) the text reads "Step {n} of 4".

### DS-4.15 Tray menu [Code `tray.rs`]
See page 16. Items: a disabled status line ("{state word} · {summary}"), separator, "Open Selene Retrieval",
"Quit". Left click opens the window. The tooltip is "Selene Retrieval: {summary}", at most 120 characters.

### DS-4.16 Collapsible / disclosure [Code `ui/collapsible.tsx`]
A bordered `rounded-lg` box. The trigger is a full-width button with a `ChevronRight` that rotates when open and the
label "Advanced". It is closed by default, operated with Enter/Space, and exposes `aria-expanded` (Radix).
`<details>`/`<summary>` is used only for "Details" in the ErrorPanel.

### DS-4.17 Navigation [Code + New]
- Header links [Code]: rounded-md `px-3 py-1.5`, 14 px; active = `bg-accent font-medium`, exact match; hidden on
  `/welcome`.
- M2 items: "Overview", "Settings".
- M4 items [New] (04 M4 row: Overview, Folders, Search, Prompt Enhancer, Providers & Keys, Connected apps, Activity,
  Settings): **Overview · Folders · Search · Connected apps · Settings**, plus a "More" menu with
  **Prompt Enhancer · Providers & Keys · Activity**. That keeps five top-level items at 760 px.
  - Below 760 px everything collapses into a "Menu" button (Radix DropdownMenu).
  - Providers & Keys appears only after M3 ships. Prompt Enhancer appears only once a provider is configured,
    mirroring U17, where tools stay hidden until usable (00 D-21).

### DS-4.18 Form field [New, M3+]
Label above (14 px, 500) → input (36 px, `rounded-md`, `control-off` border, focus ring) → help or error line
(12 px). Errors show live from `Settings::validate()` (02 §2.5) as plain text in `warning-strong`, plus an
`AlertTriangle`, linked with `aria-describedby`. A password/key field masks its value and has a "Show" toggle button
(`aria-pressed`).

### DS-4.19 Managed lock [New, M5]
A control set by `policy.toml` is disabled. It shows a `Lock` icon and the line "Managed by your organization" (12 px
muted) in place of its help line (02 §2.5).

---

## DS-5 Shared status mapping (every page uses exactly these names)

Three name layers: the **engine status** (sr-common, wire names), the **UI state** (sr-app `FolderState`/`Overall`,
TS names) and the **label** the user sees. Briefs refer to UI states in `code font`, followed by the engine name in
brackets where it matters.

### DS-5.1 Folder states

| UI state | Engine status(es) | Status line (exact copy) [Code `state.rs`] | Short label (badge) [New] | Icon (DS-3) | Colour | Progress | Actions available |
|---|---|---|---|---|---|---|---|
| `waiting` | `pending` (incl. jobs queued, not started) | "Waiting to start" | Waiting | Loader2 (spin) | primary | Indeterminate | Pause, Remove… |
| `scanning` | `scanning` | "Looking through files…" or "Looking through files… {n} found" | Looking through files | Loader2 (spin) | primary | Indeterminate | Pause, Remove… |
| `indexing` | `indexing` | "Indexing {done} of {total} files" + " · {eta} left" when known; "Indexing…" if total is 0 | Indexing {pct}% | Loader2 (spin) | primary | Determinate (done/total) | Pause, Remove… |
| `up_to_date` | `synced` with 0 skipped | "Up to date · {n} files" | Up to date | CheckCircle2 | success | — | Check for changes now, Pause, Remove… |
| `up_to_date_with_skips` | `synced` with skipped > 0, `synced_with_errors`, `partial` (see note) | "Up to date · {n} files ({k} skipped)" | Up to date, {k} skipped | CheckCircle2 | success | — | as above + [New, M4] "View skipped files" |
| `paused` | `paused` | "Paused"; "Paused until you choose what to do" for a folder that was set to only list its files (Amendment 17) | Paused | PauseCircle | muted-foreground | — | Resume (that folder: "Search its contents"), Remove… |
| `unavailable` | `unavailable` | "Can't reach this folder. The drive may be unplugged or the folder moved." | Not available | AlertTriangle | warning-strong | — | Check for changes now, Remove… (no Pause) |
| `problem` | `error` | "Something went wrong with this folder." | Needs attention | AlertTriangle | warning-strong | — | Check for changes now, Pause, Remove… |
| [New] `removing` | folder removed, `PurgeCompleted` not yet received | "Removing… It no longer appears in search." | Removing | Trash2 + spin | muted-foreground | — | none (OQ-18) |
| [New] `stopped` | `partial` after a user cancel (M4) | "Stopped · {done} of {total} files. Search covers what's done." | Stopped | PauseCircle | muted-foreground | — | Continue (= Check for changes now), Remove… (OQ-37) |

Notes:
- `{eta}` uses `duration_text` [Code]: "a minute or two" (< 2 min), "about {m} minutes", "about an hour",
  "about {h} hours".
- Numbers use thousands separators (`2,100`) [Code `thousands`].
- Listed files (`skip_reason = 'files_only'`: an access grant's files, and those of a folder paused by Amendment 17)
  are **not** counted as skipped and never shown as skipped (m2-w §0 task 4c, catalog `skipped_files` excludes them).
- **Mismatch (OQ-37):** the code maps `partial` (a cancelled or partial run, "not everything is in sync yet", m2-w §5.4)
  to `up_to_date_with_skips`, which claims "Up to date". Recommendation: the new `stopped` state.

### DS-5.2 Overall state (header pill, tray, notifications) [Code]

| `Overall` | When | Pill dot | Tray icon | Tray state word | Summary examples (exact) |
|---|---|---|---|---|---|
| `starting` | Not attached yet, or the host is `starting` | muted-foreground, pulse | `tray-idle` | "Starting" | "Starting…" |
| `no_folders` | Attached, zero folders | muted-foreground | `tray-idle` | "No folders yet" | "No folders yet" |
| `indexing` | Any folder `waiting`/`scanning`/`indexing`, or catch-up running | primary, pulse | `tray-indexing` | "Indexing" | "Indexing 340 of 2,100 files", "Looking through your files…", "Catching up after restart (40%)" |
| `synced` | Everything else | success | `tray-synced` | "Up to date" | "Up to date" (also "Paused" when every folder is paused, from `plain_summary`) |
| `problem` | Service unreachable, or any folder `unavailable`/`problem` | warning-strong | `tray-problem` | "Needs attention" | the problem title (e.g. "Background service didn't start"), or "A folder needs your attention" |

Precedence [Code `state.rs view()`]: problem (service down) → starting → problem (folder) → indexing (catch-up) →
no_folders → indexing → synced.

### DS-5.3 Folder notes (modifiers, shown in addition to the state)

| Note | Condition | Copy (exact) | Icon | Source |
|---|---|---|---|---|
| Network drive | `watch_support = network` and the folder takes watcher events | "This folder is on a network drive, so changes are picked up every hour." (cadence-dependent: "every {h} hours" / "every {m} minutes") | Network | [Code] `sr-sync watch_note` (**not delivered to the app yet**, OQ-38) |
| Live updates unavailable | `watch_support = unsupported` | "Live updates aren't available for this drive, so changes are picked up every hour." | Network | [Code] `watch_note` |
| Was set to only list its files | `was_files_only` and state `paused`: the folder was in the Files-only mode, removed 2026-11-08 (Amendment 17), and stays paused until the user chooses (not shown while its drive is unplugged, when [Search its contents] isn't offered) | "This folder was set to only list its files, which is no longer possible. Search its contents, or remove it from Selene Retrieval." + [Search its contents] (= Resume) and [Remove…] | ListTree | [Code] Amendment 17 (replaces the "Files only" note: the mode and its Advanced choice are gone) |
| Online-only files (M5) | OneDrive placeholders present, `include_online_only = false` | "Some files are stored only in OneDrive and were skipped." + [Include them] | Cloud | [Doc] U19, code 11 |

### DS-5.4 Search model change (`ModelStep`) [Code `state.rs`]

| Step | Line (exact) | Progress | Buttons |
|---|---|---|---|
| (idle, upgrade available) | "Recommended for your computer: **Better search** ({size} download)" + "Downloads once from the model's official page. Search keeps working while it improves." | — | [Use it] |
| (idle, nothing better) | "Search is set up for this computer." | — | — |
| `downloading` | "Starting the download…" then "Downloading better search… {mb} of {total} MB" | Determinate (MB) | [Stop] |
| `waiting_for_unmetered` | "Paused because this connection may cost money (metered)." | — | [Download anyway] [Stop] |
| `improving` | "Improving search… (search keeps working)" / "Improving search… {pct}% (search keeps working)" | Determinate when total > 0 | [Stop] |
| `done` | "Search upgraded. Your folders now use the better search model." | — | — |
| `failed` | ErrorPanel from the code: ModelDownloadFailed (resumable: "The download stopped. It will continue where it left off."), ModelIntegrity, Offline, NetworkBlocked, DiskFull, ModelIncompatible | — | the code's one action |

### DS-5.5 Claude Desktop connection (`ClaudeState`) [Code `ClaudeConnect.tsx`]

| State | Icon | Text (exact) | Buttons |
|---|---|---|---|
| `not_connected` | — | "One click lets Claude search the folders you added. Nothing else in Claude's settings changes." | [Connect Claude Desktop] (Plug) · "Copy setup text instead" (link) |
| `connected` | CheckCircle2 success | "Claude Desktop is connected" + (after Connect) "Connected. If Claude is open, quit it and open it again to finish." or the example line "Ask Claude about your files, for example: “Search my documents for the lease agreement.”" | — |
| `needs_reconnect` | — | "Claude's connection points to an old copy of Selene Retrieval. Reconnect to fix it." | [Reconnect Claude Desktop] · "Copy setup text instead" |
| `not_installed` | — | "Claude Desktop isn't installed on this computer. After you install it, connect it here. For other AI apps, copy the setup text and paste it into that app's MCP settings." | [Copy setup text] |
| `cant_read` | — | "Claude's settings file couldn't be read, so we didn't change it. Copy the setup text and paste it into Claude's settings instead (Settings → Developer → Edit Config)." | [Copy setup text] |

### DS-5.6 Approval request states [Code catalog `ApprovalState`]
`pending` (card shown) → `approved` (card gone; the folder appears with `waiting`) / `denied` (card gone; the agent
gets "Request declined") / `expired` (after 10 min; card gone; the agent gets "Request declined… or it expired").

---

## DS-6 Accessibility baseline (applies to every page)

Target: **WCAG 2.2 AA** (M5 does the formal accessibility pass, 04 M5 row; every page is designed to it now).

- **Keyboard:** everything is reachable with Tab, in visual order. Buttons work with Enter/Space. Switches use
  Space. Radio groups use the arrow keys. Esc closes dialogs and menus. Focus is always visible (2 px ring). Focus
  is never lost after an async action: after removing a folder, focus moves to the next folder card, or to "Add a
  folder" when none is left.
- **Focus appearance (2.4.11/2.4.13):** the ring is 2 px `primary` at 50 % alpha. **Change:** use a solid 2 px
  `primary` ring with a 2 px `background` offset (`ring-offset-2`), so it reaches 3:1 on every surface (primary on
  background 11.96 / 10.75).
- **Screen readers:**
  - Status changes announce through one polite live region per page. It is fed only when the folder **state**
    changes (not on every progress tick), e.g. "corpus: Up to date, 49 files".
  - Errors use `role="alert"` (DS-4.7). Progress bars have names and value text (DS-4.5).
  - Headings: one `h1` per page ("Your folders", "Settings"); cards are `h2` [Code].
  - The header brand is not a heading. **Change:** the header becomes a `<header>` landmark with
    `<nav aria-label="Main">`.
- **Reduced motion:** DS-1.6.
- **High contrast (Windows contrast themes, `forced-colors: active`):**
  - Rely on system colours: buttons `ButtonText`/`ButtonFace`, links `LinkText`, focus `Highlight`.
  - Status dots and progress fills get `forced-color-adjust: none` with `Highlight` (fill) and `CanvasText` (track
    border), so they stay visible.
  - Cards and panels keep a 1 px `CanvasText` border, because tints disappear in forced colours.
  - Icons inherit `currentColor`, so they follow the theme.
  - State never depends on colour (DS-5).
- **Text scaling:** layouts hold at 200 % text size at the minimum window (640 × 540): action rows wrap, the header
  pill truncates, nothing is clipped. Truncated names keep their full text in `title`, plus a visually hidden full
  name for screen readers.
- **Target size (2.5.8):** at least 24 × 24 px (all buttons are 32 px or more tall). Inline links in sentences are
  exempt.
- **Language:** `<html lang="en">` until localisation (OQ-4).
- **Timing:** approval requests expire after 10 min (product rule). The card shows "Expires in about {m} minutes" once
  under 5 min remain, so the limit is visible (WCAG 2.2.1). [New]

---

## DS-7 Voice and copy rules (short form; full record in 00 D-13 – D-15)

1. Say what happened, then the one next step. Use the error table (02 §2.5, `messages.rs`) verbatim; never write a new
   error string in the UI.
2. Never say "daemon", "MCP", "index", "embedding", "vector", "tier", "EP", "collection", "token" in Basic surfaces.
   Allowed words: "search", "folder", "background service", "search model" (Settings only), "setup text".
   - **Exception [Code]:** "Indexing {n} of {m} files" is the established status word. The user test's S5 lists
     "index" as jargon, so watch it (OQ-13).
3. Quote folder and file names. Use thousands separators. Give durations in words.
4. Buttons are verbs naming the result: "Connect Claude Desktop", "Choose a subfolder", "Use it", "Download anyway".
5. Reassure about privacy in plain words where it matters: "Your files are not uploaded anywhere." [Code].
6. Raw codes appear only under "Details".
