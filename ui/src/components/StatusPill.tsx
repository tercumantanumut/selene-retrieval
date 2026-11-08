import type { Overall } from "@/bindings";
import { cn } from "@/lib/utils";

// DS-5.2. The "needs attention" dot uses `warning-strong` (3:1 non-text contrast, OQ-11). Pulses
// stop under reduced motion (index.css); forced colours paint the dot with Highlight.
const STYLE: Record<Overall, { dot: string; label: string }> = {
  starting: { dot: "bg-muted-foreground animate-pulse", label: "Starting" },
  no_folders: { dot: "bg-muted-foreground", label: "No folders yet" },
  indexing: { dot: "bg-primary animate-pulse", label: "Indexing" },
  synced: { dot: "bg-success", label: "Up to date" },
  problem: { dot: "bg-warning-strong", label: "Needs attention" },
};

export function StatusPill({ overall, summary }: { overall: Overall; summary: string }) {
  const s = STYLE[overall];
  return (
    // Page 04 [New]: focusable; the full summary is its text (truncated only visually). Not a live
    // region: it would chatter during indexing (page 03).
    <span
      tabIndex={0}
      className="inline-flex max-w-full items-center gap-2 rounded-full border border-border px-3 py-1 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
      title={summary}
    >
      <span data-status-dot className={cn("size-2 shrink-0 rounded-full", s.dot)} aria-hidden />
      <span className="truncate">{summary || s.label}</span>
    </span>
  );
}
