// Amendment 13 · "Create a folder for me" (docs/07-default-folder-plan.md). The app (not this page)
// decides the folder: `Documents\Selene Retrieval`, made as the user and added like any folder. The
// page only asks whether to offer it and presses the button; it never sends a path.
import { useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { commands, type DefaultFolderOffer, type DefaultFolderResult, type UiError } from "@/bindings";
import { asUiError, unwrap, useAppView } from "@/lib/app";

export const DEFAULT_FOLDER_KEY = ["default-folder"] as const;

/** The button's description (the folder's name is the app's). */
export function defaultFolderHint(name: string): string {
  return `Makes a “${name}” folder in Documents. Anything you put there becomes searchable.`;
}

/**
 * Whether to offer [Create a folder for me]. Read again only when the pushed folder list or the
 * organisation's policy changes (never on a timer). `undefined` until known or when not offered.
 */
export function useDefaultFolder(): DefaultFolderOffer | undefined {
  const view = useAppView();
  const sig = [
    ...(view?.folders ?? []).map((f) => `${f.id}:${f.state}:${f.path ?? ""}`),
    ...(view?.policy?.managed ?? []).map((m) => m.path),
  ].join("|");
  const q = useQuery({
    queryKey: [...DEFAULT_FOLDER_KEY, sig],
    queryFn: () => commands.getDefaultFolder(),
    enabled: view !== undefined,
  });
  return q.data ?? undefined;
}

/** [Create a folder for me]: busy state, the plain error (with its one action), the result. */
export function useCreateDefaultFolder() {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<UiError | null>(null);
  const create = async (): Promise<DefaultFolderResult | null> => {
    setBusy(true);
    setError(null);
    try {
      return await unwrap(commands.createDefaultFolder());
    } catch (e) {
      setError(asUiError(e));
      return null;
    } finally {
      setBusy(false);
    }
  };
  return { busy, error, create, clearError: () => setError(null) };
}
