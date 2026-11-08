import { useEffect, useRef, useState } from "react";
import { Link } from "@tanstack/react-router";
import { commands, type FolderView } from "@/bindings";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Progress } from "@/components/ui/progress";
import { unwrap, useErrorToast } from "@/lib/app";
import { AlertTriangle, CheckCircle2, Cloud, FolderOpen, ListTree, Loader2, Network, PauseCircle, Trash2 } from "lucide-react";

function percent(f: FolderView): number | null {
  if (f.files_total <= 0) return null;
  return Math.round((100 * Math.min(f.files_done, f.files_total)) / f.files_total);
}

export function isBusy(f: FolderView): boolean {
  return f.state === "waiting" || f.state === "scanning" || f.state === "indexing";
}

/**
 * DS-5.3 (Amendment 17): a folder that was set to only list its files, a removed mode, stays paused
 * until the user chooses: [Search its contents] (resume) or [Remove…] (exact copy).
 */
export const WAS_FILES_ONLY_NOTE =
  "This folder was set to only list its files, which is no longer possible. Search its contents, or remove it from Selene Retrieval.";
/** The paused folder's [Resume] in that case. */
export const SEARCH_ITS_CONTENTS = "Search its contents";
/** DS-5.3 online-only note (exact copy); [Include them] lives on the folder's detail page. */
export const ONLINE_ONLY_NOTE = "Some files are stored only in OneDrive and were skipped.";

/** DS-5.1 state icon (decorative: the status line carries the state). */
export function FolderStateIcon({ f }: { f: FolderView }) {
  switch (f.state) {
    case "up_to_date":
    case "up_to_date_with_skips":
      return <CheckCircle2 className="size-5 shrink-0 text-success" aria-hidden />;
    case "unavailable":
    case "problem":
      return <AlertTriangle className="size-5 shrink-0 text-warning-strong" aria-hidden />;
    case "paused":
    case "stopped":
      return <PauseCircle className="size-5 shrink-0 text-muted-foreground" aria-hidden />;
    case "removing":
      return (
        <span className="relative size-5 shrink-0 text-muted-foreground" aria-hidden>
          <Trash2 className="size-5" />
          <Loader2 className="absolute -bottom-1 -right-1 size-3 animate-spin" />
        </span>
      );
    default:
      return <Loader2 className="size-5 shrink-0 animate-spin text-primary" aria-hidden />;
  }
}

export function FolderProgress({ folder }: { folder: FolderView }) {
  return isBusy(folder) ? (
    <Progress value={percent(folder)} label={folder.name} valueText={folder.line} />
  ) : null;
}

/** Moves focus after a folder leaves the list: the next card's first action, else "Add a folder". */
function focusAfterRemove(from: HTMLElement | null) {
  const li = from?.closest("li");
  const next = (li?.nextElementSibling ?? li?.previousElementSibling)?.querySelector<HTMLElement>("button");
  (next ?? document.getElementById("add-folder"))?.focus();
}

export function FolderCard({ folder, compact = false }: { folder: FolderView; compact?: boolean }) {
  const showError = useErrorToast();
  const [confirm, setConfirm] = useState(false);
  const [sending, setSending] = useState(false);
  const keepRef = useRef<HTMLButtonElement>(null);
  const removeRef = useRef<HTMLButtonElement>(null);
  const cardRef = useRef<HTMLDivElement>(null);
  const reopened = useRef(false);
  const busy = isBusy(folder);
  const removing = folder.state === "removing";

  // Confirming: focus the least destructive choice (DS-4.9). Back to idle: focus "Remove…" again.
  useEffect(() => {
    if (confirm) keepRef.current?.focus();
    else if (reopened.current) removeRef.current?.focus();
  }, [confirm]);

  const sync = () => {
    unwrap(commands.syncFolder(folder.id)).catch((e) => showError(e, sync));
  };
  const setPaused = (paused: boolean) => {
    unwrap(commands.setFolderPaused(folder.id, paused)).catch((e) => showError(e, () => setPaused(paused)));
  };
  const keep = () => {
    reopened.current = true;
    setConfirm(false);
  };
  const remove = () => {
    setSending(true);
    unwrap(commands.removeFolder(folder.id))
      .then(() => {
        setConfirm(false);
        focusAfterRemove(cardRef.current);
      })
      .catch((e) => showError(e, remove))
      .finally(() => setSending(false));
  };

  return (
    <Card
      ref={cardRef}
      role="group"
      aria-label={`${folder.name}, ${folder.line}`}
      aria-busy={removing || undefined}
      // "Removing…" stays fully readable (no fading text, WCAG 1.4.3); the icon and line say it.
      className="space-y-3"
    >
      <div className="flex items-start gap-3">
        <FolderStateIcon f={folder} />
        <div className="min-w-0 flex-1">
          <p className="truncate font-medium" title={folder.name}>
            <FolderOpen className="mr-1.5 inline size-4 text-muted-foreground" aria-hidden />
            {compact || removing ? (
              folder.name
            ) : (
              // M4: the name opens the folder's detail (page 25).
              <Link
                to="/folders/$id"
                params={{ id: folder.id }}
                search={{}}
                className="rounded-sm hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
              >
                {folder.name}
              </Link>
            )}
          </p>
          {folder.path && (
            <p className="truncate text-xs text-muted-foreground" title={folder.path}>
              {folder.path}
            </p>
          )}
          <p className="mt-1 text-sm text-muted-foreground">{folder.line}</p>
          {folder.note && (
            <p className="mt-1 flex items-start gap-1.5 text-xs text-muted-foreground">
              <Network className="mt-px size-3.5 shrink-0" aria-hidden />
              {folder.note.text}
            </p>
          )}
          {folder.was_files_only && folder.state === "paused" && (
            // Only while [Search its contents] is offered (not while the drive is away).
            <p className="mt-1 flex items-start gap-1.5 text-xs text-muted-foreground">
              <ListTree className="mt-px size-3.5 shrink-0" aria-hidden />
              {WAS_FILES_ONLY_NOTE}
            </p>
          )}
          {folder.cloud_note && (
            <p className="mt-1 flex items-start gap-1.5 text-xs text-muted-foreground">
              <Cloud className="mt-px size-3.5 shrink-0" aria-hidden />
              {folder.cloud_note}
            </p>
          )}
          {folder.online_only_skipped > 0 && !removing && (
            <p className="mt-1 flex items-start gap-1.5 text-xs text-muted-foreground">
              <Cloud className="mt-px size-3.5 shrink-0" aria-hidden />
              <span>
                {ONLINE_ONLY_NOTE}{" "}
                {!compact && (
                  <Link
                    to="/folders/$id"
                    params={{ id: folder.id }}
                    search={{}}
                    className="rounded-sm text-primary underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
                  >
                    Include them
                  </Link>
                )}
              </span>
            </p>
          )}
        </div>
      </div>
      <FolderProgress folder={folder} />
      {!compact && !removing && (
        <div className="flex flex-wrap items-center gap-2">
          {confirm ? (
            <div
              role="group"
              aria-label={`Remove “${folder.name}”?`}
              className="flex flex-wrap items-center gap-2"
              onKeyDown={(e) => {
                if (e.key === "Escape") keep();
              }}
            >
              <span className="text-sm">
                Stop searching this folder? It disappears from search right away. Your files stay where they are.
              </span>
              <Button size="sm" variant="danger" busy={sending} onClick={remove}>
                Remove
              </Button>
              <Button ref={keepRef} size="sm" variant="ghost" disabled={sending} onClick={keep}>
                Keep
              </Button>
            </div>
          ) : (
            <>
              {folder.state === "stopped" ? (
                <Button size="sm" variant="outline" onClick={sync}>
                  Continue
                </Button>
              ) : (
                !busy &&
                folder.state !== "paused" && (
                  <Button size="sm" variant="outline" onClick={sync}>
                    Check for changes now
                  </Button>
                )
              )}
              {folder.state === "paused" ? (
                <Button size="sm" variant="outline" onClick={() => setPaused(false)}>
                  {folder.was_files_only ? SEARCH_ITS_CONTENTS : "Resume"}
                </Button>
              ) : (
                folder.state !== "unavailable" &&
                folder.state !== "stopped" && (
                  <Button size="sm" variant="ghost" onClick={() => setPaused(true)}>
                    Pause
                  </Button>
                )
              )}
              <Button ref={removeRef} size="sm" variant="ghost" onClick={() => setConfirm(true)}>
                Remove…
              </Button>
              {folder.state === "up_to_date_with_skips" && folder.files_skipped > 0 && (
                // DS-5.1 [New, M4]: the skipped list lives in the folder detail.
                <Button asChild size="sm" variant="link">
                  <Link to="/folders/$id" params={{ id: folder.id }} search={{ section: "skipped" }}>
                    View skipped files
                  </Link>
                </Button>
              )}
            </>
          )}
        </div>
      )}
    </Card>
  );
}
