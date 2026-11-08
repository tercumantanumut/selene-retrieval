// Page 32 · Move the search index (Advanced → Index location → [Move…]). The service checks the
// target, copies, verifies, switches and deletes the old copy only after a good start; progress
// and the result are pushed (`AppView.index_move`, `ServicePush index_move_*`).
import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { commands, type MoveCheckView, type UiError } from "@/bindings";
import { ErrorPanel } from "@/components/ErrorPanel";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { Progress } from "@/components/ui/progress";
import { asUiError, INDEX_LOCATION_KEY, unwrap, useAppView } from "@/lib/app";
import { canRetry } from "@/lib/errors";
import { pickFolder } from "@/lib/folders";
import { indexMoveResultStore, useStore } from "@/lib/store";
import { AlertTriangle, CheckCircle2, FolderSearch, Loader2 } from "lucide-react";

export function MoveIndexDialog({ onClose }: { onClose: () => void }) {
  const view = useAppView();
  const result = useStore(indexMoveResultStore);
  const now = useQuery({ queryKey: INDEX_LOCATION_KEY, queryFn: () => unwrap(commands.getIndexLocation()) });
  const [target, setTarget] = useState<string | null>(null);
  const [check, setCheck] = useState<MoveCheckView | null>(null);
  const [checking, setChecking] = useState(false);
  const [started, setStarted] = useState(false);
  const [error, setError] = useState<UiError | null>(null);

  const moving = view?.index_move ?? null;
  const running = started && (moving !== null || view?.restarting === true || view?.overall === "starting");
  const failed = started && result?.kind === "failed" ? result : null;
  const done = started && result?.kind === "done";
  useEffect(() => {
    if (done) {
      // "Search index moved." is toasted by the push listener; the dialog has done its job.
      indexMoveResultStore.set(null);
      onClose();
    }
  }, [done, onClose]);

  const choose = async () => {
    const p = await pickFolder(target ?? undefined);
    if (!p) return;
    setTarget(p);
    setCheck(null);
    setError(null);
    setChecking(true);
    try {
      setCheck(await unwrap(commands.checkIndexTarget(p)));
    } catch (e) {
      setError(asUiError(e));
    } finally {
      setChecking(false);
    }
  };
  const move = async () => {
    if (!target) return;
    setError(null);
    indexMoveResultStore.set(null);
    try {
      await unwrap(commands.moveIndex(target));
      setStarted(true);
    } catch (e) {
      setError(asUiError(e));
    }
  };
  const cancelMove = () => unwrap(commands.cancelIndexMove()).catch((e) => setError(asUiError(e)));

  const busy = running && !failed;
  const canCancelMove = busy && (moving?.can_cancel ?? false);

  return (
    <Modal
      open
      busy={busy}
      onClose={onClose}
      title="Move the search index"
      footer={
        <>
          <Button
            variant="ghost"
            disabled={busy && !canCancelMove}
            onClick={() => (canCancelMove ? void cancelMove() : onClose())}
          >
            Cancel
          </Button>
          <Button disabled={!check?.ok || busy || checking} onClick={() => void move()}>
            Move
          </Button>
        </>
      }
    >
      <div className="space-y-3 text-sm" aria-busy={busy || undefined}>
        {now.error && <ErrorPanel error={asUiError(now.error)} onRetry={() => void now.refetch()} />}
        {now.data && (
          <p>
            <span className="text-muted-foreground">Now:</span> <code className="break-all text-xs">{now.data.path}</code> ·{" "}
            {now.data.size}
          </p>
        )}
        {!busy && (
          // After a failure the chooser stays: [Retry] tries the same place again, or pick another.
          <div className="space-y-2">
            <Button variant="outline" size="sm" onClick={() => void choose()} disabled={checking}>
              <FolderSearch /> Choose a new location…
            </Button>
            {target && check && (
              <p>
                <span className="text-muted-foreground">New:</span>{" "}
                <code className="break-all text-xs">{check.index_path || target}</code>
                {check.free && ` · ${check.free} free`}
              </p>
            )}
            {checking && (
              <p className="flex items-center gap-2 text-muted-foreground" role="status">
                <Loader2 className="size-4 animate-spin" aria-hidden /> Checking…
              </p>
            )}
            {check && (
              <p role="status" className="flex items-center gap-2">
                {check.problem ? (
                  <>
                    <AlertTriangle className="size-4 shrink-0 text-warning-strong" aria-hidden />
                    <span className="text-warning-strong">{check.problem}</span>
                  </>
                ) : (
                  <>
                    <CheckCircle2 className="size-4 shrink-0 text-success" aria-hidden /> Enough space
                  </>
                )}
              </p>
            )}
          </div>
        )}
        {busy && (
          // Not a live region: the line changes on every pushed step. The shell's live region
          // announces it once per 25 % (DS-6); the bar carries the current value for queries.
          <div className="space-y-2">
            {moving ? (
              <>
                <p id="move-line">{moving.line}</p>
                <Progress value={moving.percent} label="Moving the search index" valueText={moving.line} />
              </>
            ) : (
              <p className="flex items-center gap-2">
                <Loader2 className="size-4 animate-spin" aria-hidden /> Starting Selene Retrieval…
              </p>
            )}
          </div>
        )}
        {failed && (
          // Page 32 "Failed": the code's panel + "Nothing was moved…" + [Retry] (M4 tester P2:
          // the panel offered the code's own action instead, e.g. "Free up space").
          <ErrorPanel
            error={canRetry(failed.error) ? { ...failed.error, action: { kind: "retry" }, action_label: "Retry" } : failed.error}
            onRetry={() => void move()}
          >
            <p className="text-sm">Nothing was moved. Your index is still in {failed.keptPath}.</p>
          </ErrorPanel>
        )}
        {error && <ErrorPanel error={error} onRetry={() => void (started || !target ? choose() : move())} />}
        <p>
          Selene Retrieval stops for a moment, moves the index, and starts again. Search is paused while it moves. Your
          own files aren't touched.
        </p>
      </div>
    </Modal>
  );
}
