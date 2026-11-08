import { useEffect, useEffectEvent, useRef, useState, type ReactNode } from "react";
import { toast } from "sonner";
import { commands, type Estimate, type UiError } from "@/bindings";
import { ErrorPanel } from "@/components/ErrorPanel";
import { FolderRootsHint } from "@/components/Managed";
import { Button } from "@/components/ui/button";
import { Card, CardText, CardTitle } from "@/components/ui/card";
import { asUiError, unwrap } from "@/lib/app";
import { baseName, pickFolder } from "@/lib/folders";
import { addFolderStore, useStore } from "@/lib/store";
import { Loader2 } from "lucide-react";

type AddState =
  | { kind: "counting"; path: string }
  | { kind: "big"; path: string; est: Estimate }
  | { kind: "error"; err: UiError; retry: () => void }
  | null;

/**
 * Page 09: add a folder without leaving the page (Overview and Folders). Same checks as the
 * wizard (refusals, the ≥ 500k big-folder choice, OQ-17), shown inline. A `choose_folder` /
 * `choose_subfolder` error action elsewhere opens the picker here (OQ-16). After a successful add,
 * focus moves to the new folder (`data-folder-id` element's first focusable group or link).
 */
export function useAddFolderFlow(): { panel: ReactNode } {
  const [state, setState] = useState<AddState>(null);
  // The new folder's id until its card or row has received focus (a ref: no extra render).
  const focusId = useRef<string | null>(null);
  const request = useStore(addFolderStore);
  const handled = useRef<number | null>(null);

  const add = async (path: string, est: Estimate) => {
    try {
      const id = await unwrap(commands.addFolder(path));
      setState(null);
      focusId.current = id;
      toast.success("Folder added", { description: est.text });
    } catch (e) {
      setState({ kind: "error", err: asUiError(e), retry: () => void add(path, est) });
    }
  };
  const start = async (defaultPath?: string) => {
    const path = await pickFolder(defaultPath);
    if (!path) return;
    setState({ kind: "counting", path });
    try {
      const est = await unwrap(commands.estimateFolder(path));
      if (est.capped && est.files >= 500_000) setState({ kind: "big", path, est });
      else await add(path, est);
    } catch (e) {
      setState({ kind: "error", err: asUiError(e), retry: () => void start(defaultPath) });
    }
  };
  const onRequest = useEffectEvent((defaultPath?: string) => void start(defaultPath));

  useEffect(() => {
    if (request && handled.current !== request.nonce) {
      handled.current = request.nonce;
      addFolderStore.set(null);
      onRequest(request.defaultPath);
    }
  }, [request]);

  // Runs after every render: the new folder appears with a later push.
  useEffect(() => {
    const id = focusId.current;
    if (!id) return;
    const host = document.querySelector<HTMLElement>(`[data-folder-id="${CSS.escape(id)}"]`);
    const el = host?.querySelector<HTMLElement>('[role="group"], a');
    if (el) {
      if (el.getAttribute("role") === "group") el.tabIndex = -1;
      el.focus();
      focusId.current = null;
    }
  });

  const s = state;
  const panel = (
    <>
      {s?.kind === "counting" && (
        <p className="flex items-center gap-2 text-sm" role="status">
          <Loader2 className="size-4 animate-spin" aria-hidden /> Counting files in “{baseName(s.path)}”…
        </p>
      )}
      {s?.kind === "big" && (
        <Card className="space-y-3">
          <CardTitle>This is a very big folder</CardTitle>
          <CardText>{s.est.text}</CardText>
          <div className="flex flex-wrap gap-3">
            <Button size="sm" onClick={() => void start(s.path)}>
              Choose a smaller folder
            </Button>
            <Button size="sm" variant="outline" onClick={() => void add(s.path, s.est)}>
              Add it anyway
            </Button>
          </div>
        </Card>
      )}
      {s?.kind === "error" && (
        <div
          onKeyDown={(e) => {
            if (e.key === "Escape") setState(null);
          }}
        >
          <ErrorPanel
            error={s.err}
            onAction={() => {
              const a = s.err.action;
              if (a.kind === "choose_subfolder") void start(a.path);
              else if (a.kind === "choose_folder") void start();
              else s.retry();
            }}
            onDismiss={() => setState(null)}
          >
            <FolderRootsHint details={s.err.details} />
          </ErrorPanel>
        </div>
      )}
    </>
  );
  return { panel };
}
