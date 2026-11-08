// R34 · "Search index not found": the search index was moved to a drive or folder that can't be
// reached now. The service waits in its recovery mode (nothing is deleted or forked) and starts
// by itself once the drive is back; this screen offers the other two ways out. Title and body are
// the error renderer's text for the dedicated code (never written here); the three choices are
// the [New] copy recorded in docs/research/m5-a-app.md.
import { useEffect, useRef, useState } from "react";
import { commands, type IndexMissingView, type RecoverChoice, type UiError } from "@/bindings";
import { ErrorPanel } from "@/components/ErrorPanel";
import { Button } from "@/components/ui/button";
import { Card, CardText } from "@/components/ui/card";
import { asUiError, unwrap } from "@/lib/app";
import { thousands } from "@/lib/format";
import { AlertTriangle, HardDrive, RotateCcw, Sparkles } from "lucide-react";

function Option({
  icon: Icon,
  title,
  children,
}: {
  icon: typeof HardDrive;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <li className="flex items-start gap-3 py-3 first:pt-0 last:pb-0">
      <Icon className="mt-0.5 size-5 shrink-0 text-muted-foreground" aria-hidden />
      <div className="min-w-0 flex-1 space-y-2">
        <h2 className="text-base font-semibold leading-tight">{title}</h2>
        {children}
      </div>
    </li>
  );
}

export function MissingIndexPanel({ missing }: { missing: IndexMissingView }) {
  const [busy, setBusy] = useState<RecoverChoice | null>(null);
  const [confirm, setConfirm] = useState(false);
  const [error, setError] = useState<{ err: UiError; choice: RecoverChoice } | null>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const keepRef = useRef<HTMLButtonElement>(null);
  const freshRef = useRef<HTMLButtonElement>(null);
  const reopened = useRef(false);

  // The screen replaces the page: its title takes focus once, so keyboard and screen-reader
  // users start here (never a button: nothing happens on a stray Enter).
  useEffect(() => {
    headingRef.current?.focus();
  }, []);
  useEffect(() => {
    if (confirm) keepRef.current?.focus();
    else if (reopened.current) freshRef.current?.focus();
  }, [confirm]);

  const recover = async (choice: RecoverChoice) => {
    setBusy(choice);
    setError(null);
    try {
      // On success the pushed view shows "Restarting the background service…".
      await unwrap(commands.recoverIndex(choice));
    } catch (e) {
      setError({ err: asUiError(e), choice });
    } finally {
      setBusy(null);
      setConfirm(false);
    }
  };
  const drive = missing.drive ?? "the drive";
  const n = missing.folders_to_restore;

  return (
    <Card className="space-y-4" role="region" aria-labelledby="index-missing-h">
      <div className="flex items-start gap-3">
        <AlertTriangle className="mt-0.5 size-5 shrink-0 text-warning-strong" aria-hidden />
        <div className="min-w-0 space-y-1">
          <h1 id="index-missing-h" ref={headingRef} tabIndex={-1} className="text-lg font-semibold leading-tight focus:outline-none">
            {missing.error.title}
          </h1>
          <p className="text-sm">{missing.error.body}</p>
        </div>
      </div>
      {error && <ErrorPanel error={error.err} onRetry={() => void recover(error.choice)} />}
      <ul className="divide-y divide-border">
        <Option icon={HardDrive} title="Reconnect the drive">
          <CardText>
            Plug in {drive} again, or put the folder back where it was. Selene Retrieval starts by itself when it finds
            the index.
          </CardText>
          <p className="break-all text-xs text-muted-foreground">
            Index: <code>{missing.path}</code>
          </p>
        </Option>
        {missing.old_copy && (
          <Option icon={RotateCcw} title="Move the index back">
            <CardText>
              Use the earlier copy on this computer. Changes made since the move are picked up again.
            </CardText>
            <p className="break-all text-xs text-muted-foreground">
              Earlier copy: <code>{missing.old_copy}</code>
            </p>
            <Button variant="outline" size="sm" busy={busy === "move_back"} disabled={busy !== null} onClick={() => void recover("move_back")}>
              Move the index back
            </Button>
          </Option>
        )}
        <Option icon={Sparkles} title="Start fresh">
          <CardText>
            {n > 0
              ? `Build a new search index on this computer and read your ${thousands(n)} folder${n === 1 ? "" : "s"} again.`
              : "Build a new search index on this computer and read your folders again."}{" "}
            Your own files aren't touched.
          </CardText>
          {confirm ? (
            <div
              role="group"
              aria-label="Start fresh?"
              className="flex flex-wrap items-center gap-2"
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  reopened.current = true;
                  setConfirm(false);
                }
              }}
            >
              <span className="text-sm">Start fresh? Search finds files again as they are read, which can take a while.</span>
              <Button size="sm" busy={busy === "start_fresh"} disabled={busy !== null} onClick={() => void recover("start_fresh")}>
                Start fresh
              </Button>
              <Button
                ref={keepRef}
                size="sm"
                variant="ghost"
                disabled={busy !== null}
                onClick={() => {
                  reopened.current = true;
                  setConfirm(false);
                }}
              >
                Keep waiting
              </Button>
            </div>
          ) : (
            <Button ref={freshRef} variant="outline" size="sm" disabled={busy !== null} onClick={() => setConfirm(true)}>
              Start fresh…
            </Button>
          )}
        </Option>
      </ul>
    </Card>
  );
}
