// DS-6 "one polite live region per page, fed only when a state changes (not on every progress
// tick)". Everything pushed (folder states, the index move, a search model change, the
// background service restarting) is turned into at most one short sentence per *change of state*
// or per coarse step (every 25 %), so screen readers aren't flooded during indexing. No timer is
// involved: throttling is by state and step, never by time (P1.5, no polling).
import type { AppView, FolderView } from "@/bindings";
import { createStore } from "@/lib/store";

/** The latest message; `n` changes on every announcement so the same text is read again. */
export const announceStore = createStore<{ text: string; n: number }>({ text: "", n: 0 });

export function announce(text: string) {
  if (!text) return;
  const cur = announceStore.get();
  announceStore.set({ text, n: cur.n + 1 });
}

/** DS-5.1 short label for the busy states (their status lines change on every tick). */
function shortLabel(f: FolderView): string {
  switch (f.state) {
    case "waiting":
      return "Waiting to start";
    case "scanning":
      return "Looking through files";
    case "indexing":
      return "Indexing";
    default:
      return f.line;
  }
}

/** "“corpus”: Up to date · 49 files" (DS-6 example), or the short label while busy. */
export function folderSentence(f: FolderView): string {
  return `“${f.name}”: ${shortLabel(f)}`;
}

const STEP = 25;
const MAX_FOLDER_SENTENCES = 3;

/**
 * What changed between two pushed views, as sentences to announce (empty = nothing worth
 * saying). Pure, so it is unit-tested; `RootLayout` feeds every pushed view through it.
 */
export function changes(before: AppView | undefined, after: AppView): string[] {
  if (!before) return [];
  const out: string[] = [];
  const prev = new Map(before.folders.map((f) => [f.id, f]));
  const moved: string[] = [];
  for (const f of after.folders) {
    const p = prev.get(f.id);
    // New folders are confirmed by their own toast ("Folder added"); only state changes here.
    if (p && p.state !== f.state) moved.push(folderSentence(f));
  }
  // Many folders at once (a restart, "check all"): the overall summary instead of a list.
  if (moved.length > MAX_FOLDER_SENTENCES) out.push(after.summary);
  else out.push(...moved);
  // Removed folders: the purge finished.
  for (const p of before.folders) {
    if (p.state === "removing" && !after.folders.some((f) => f.id === p.id)) {
      out.push(`“${p.name}” was removed from search.`);
    }
  }
  if (!before.restarting && after.restarting) out.push("Restarting the background service…");
  // Index move (page 32): its phase line once per 25 % step and when it ends.
  const bm = before.index_move;
  const am = after.index_move;
  if (am && (!bm || Math.floor(am.percent / STEP) !== Math.floor(bm.percent / STEP) || am.can_cancel !== bm.can_cancel)) {
    out.push(am.line);
  }
  // Search model change: each new step once (the text of a running step changes every tick).
  const bc = before.model_change;
  const ac = after.model_change;
  if (ac && (!bc || bc.step !== ac.step)) out.push(ac.text);
  // The service came back / went away: the header pill isn't a live region (page 03).
  if (before.overall !== after.overall && (after.overall === "problem" || before.overall === "problem")) {
    out.push(after.summary);
  }
  return out;
}
