import { useEffect, useRef } from "react";
import type { AppView } from "@/bindings";
import { announce, announceStore, changes } from "@/lib/announce";
import { useStore } from "@/lib/store";

/**
 * The shell's one polite live region (DS-6). It exists from the first render (an empty region
 * that later gets text is announced reliably; one inserted with its text often isn't). The text
 * alternates between two identical nodes keyed by the announcement number, so the same sentence
 * twice in a row is still read.
 */
export function LiveRegion() {
  const { text, n } = useStore(announceStore);
  return (
    <div className="sr-only" role="status" aria-live="polite" aria-atomic="true" data-testid="live-region">
      {text && <span key={n}>{text}</span>}
    </div>
  );
}

/** Feeds the live region from pushed views: state changes and coarse steps only (no timer). */
export function useStateAnnouncements(view: AppView | undefined) {
  const last = useRef<AppView | undefined>(undefined);
  useEffect(() => {
    if (!view) return;
    const said = changes(last.current, view);
    last.current = view;
    // One region, one message: join this push's sentences (a burst is coalesced by the app).
    if (said.length > 0) announce(said.join(" "));
  }, [view]);
}
