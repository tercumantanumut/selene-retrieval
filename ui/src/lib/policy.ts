// Page 33 · settings managed by the organisation (`policy.toml` / registry policy, M5-E). The
// service lists every locked setting in the status snapshot (`AppView.policy.managed`, pushed);
// the UI disables those controls, keeps them focusable (`aria-disabled`) and shows the reason.
// Policy only ever takes something away, so a locked control never hides a security default.
import type { AppView, ManagedFieldView } from "@/bindings";
import { useAppView } from "@/lib/app";

/** The one plain line next to a locked setting (= sr_common `MANAGED_TEXT`). */
export const MANAGED_TEXT = "Managed by your organisation";
/** Settings header row when anything is managed (page 33). */
export const SOME_MANAGED_TEXT = "Some settings are managed by your organisation.";

/** Settings paths (dotted `settings.toml` paths, or M5-E's pseudo paths for locks without one). */
export const PATHS = {
  remoteProviders: "provider.remote",
  allowedProviders: "provider.provider",
  http: "integrations.http_server_enabled",
  folderRequests: "integrations.mcp_add_folder_enabled",
  folderChecks: "integrations.mcp_sync_folder_enabled",
  folderRoots: "folders.allowed_roots",
  modelDownloads: "search.model_downloads",
  searchModel: "search.search_model",
  useGraphicsCard: "search.use_graphics_card",
  answersEnabled: "answers.answers_enabled",
  deeperAnswers: "answers.deeper_answers",
  maxRounds: "answers.max_rounds",
  startWithWindows: "general.start_with_windows",
  dataDir: "system.data_dir",
} as const;

function covers(m: string, path: string): boolean {
  const p = m.endsWith(".*") ? m.slice(0, -2) : m;
  return path === p || path.startsWith(`${p}.`);
}

/** The managed entry covering `path` (a section entry covers its fields), or `null`. */
export function managedField(view: AppView | undefined, path: string): ManagedFieldView | null {
  return view?.policy?.managed.find((m) => covers(m.path, path)) ?? null;
}

export type Policy = {
  /** The managed entry for the first of `paths` that is managed. */
  field: (...paths: string[]) => ManagedFieldView | null;
  /** Anything at all is managed (the Settings header row). */
  any: boolean;
};

export function usePolicy(): Policy {
  const view = useAppView();
  return {
    field: (...paths) => {
      for (const p of paths) {
        const f = managedField(view, p);
        if (f) return f;
      }
      return null;
    },
    any: (view?.policy?.managed.length ?? 0) > 0,
  };
}
