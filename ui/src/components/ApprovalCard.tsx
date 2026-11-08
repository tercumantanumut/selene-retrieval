import { useEffect, useRef, useState } from "react";
import { toast } from "sonner";
import { commands, type ApprovalView } from "@/bindings";
import { Button } from "@/components/ui/button";
import { unwrap, useErrorToast } from "@/lib/app";
import { baseName } from "@/lib/folders";
import { FolderPlus } from "lucide-react";

/** "Expires in about {m} minutes." once under 5 minutes remain (page 14). Recomputed on every
 * render, i.e. on each pushed state change; nothing ticks. */
function expiryLine(expiresAtMs: number | null): string | null {
  if (!expiresAtMs) return null;
  const left = expiresAtMs - Date.now();
  if (left <= 0 || left >= 5 * 60_000) return null;
  const m = Math.max(1, Math.ceil(left / 60_000));
  return `Expires in about ${m} minute${m === 1 ? "" : "s"}.`;
}

/**
 * A connected app (e.g. Claude) asks to add a folder. Only shown when the user turned that on in
 * Advanced; nothing is added until they click Allow here (requests expire after 10 minutes).
 * Focus is never moved into the card by itself, so a stray Enter can't approve; only when the app
 * was opened for this request (`sr-app --approve`) does focus go to the card's heading.
 */
export function ApprovalCard({
  approval,
  focus = false,
  live = "status",
  copy = false,
}: {
  approval: ApprovalView;
  focus?: boolean;
  live?: "alert" | "status";
  /** A second rendering of a card the shell already shows (Connected apps → Folder requests):
   *  no live region and no anchor id, so it is announced once and ids stay unique. */
  copy?: boolean;
}) {
  const showError = useErrorToast();
  const [busy, setBusy] = useState(false);
  const headingRef = useRef<HTMLParagraphElement>(null);

  useEffect(() => {
    if (focus) {
      headingRef.current?.scrollIntoView({ block: "nearest" });
      headingRef.current?.focus();
    }
  }, [focus]);

  const decide = async (allow: boolean) => {
    setBusy(true);
    try {
      await unwrap(commands.decideApproval(approval.id, allow));
      if (allow) {
        toast.success("Folder added", { description: `“${baseName(approval.path)}” is now searchable as it's indexed.` });
      }
    } catch (e) {
      showError(e, () => void decide(allow));
    } finally {
      setBusy(false);
    }
  };
  const expires = expiryLine(approval.expires_at_ms);
  return (
    <div role={copy ? undefined : live} data-surface className="rounded-lg border border-primary/40 bg-primary/5 p-4">
      <div className="flex items-start gap-3">
        <FolderPlus className="mt-0.5 size-5 shrink-0 text-primary" aria-hidden />
        <div className="min-w-0 flex-1 space-y-2">
          <p
            ref={headingRef}
            id={copy ? undefined : `approval-${approval.id}`}
            tabIndex={-1}
            className="font-medium focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
          >
            {approval.client} wants to add a folder
          </p>
          <p className="break-all text-sm text-muted-foreground">{approval.path}</p>
          {expires && <p className="text-sm text-muted-foreground">{expires}</p>}
          <div className="flex flex-wrap gap-2">
            <Button size="sm" disabled={busy} onClick={() => void decide(true)}>
              Allow
            </Button>
            <Button size="sm" variant="ghost" disabled={busy} onClick={() => void decide(false)}>
              Don't allow
            </Button>
          </div>
        </div>
      </div>
    </div>
  );
}
