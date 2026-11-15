// App-wide data hooks. Status is PUSHED by the app (Tauri events) into the TanStack Query cache;
// the first read is a single command call. Nothing here re-asks on a timer (see eslint no-polling):
// a pushed signal ("groups changed", "a new activity row") makes the open page read once again.
import { useEffect } from "react";
import { QueryClient, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { toast } from "sonner";
import { commands, events, type AppView, type ClientsView, type ConnectStatus, type UiError } from "@/bindings";
import { canRetry, fromWire } from "@/lib/errors";
import {
  askConsent,
  indexMoveResultStore,
  keyFieldStore,
  requestReenterKey,
  setRequestLine,
  startAddFolder,
} from "@/lib/store";
import { OS_SUBJECT, QUIT_CLAUDE_HINT } from "@/lib/platform";

export const VIEW_KEY = ["view"] as const;
export const CLAUDE_KEY = ["claude"] as const;
export const ABOUT_KEY = ["about"] as const;
/** Provider status, consents and answers: read when a page opens or after an action (never polled). */
export const PROVIDER_KEY = ["provider"] as const;
export const CONSENTS_KEY = ["consents"] as const;
export const ANSWERS_KEY = ["answers"] as const;
export const PROVIDERS_LIST_KEY = ["providers-list"] as const;
/**
 * "Claude (sign in)" / "Codex (sign in)" status per provider (`lib/cliSignIn`); set by a pushed
 * `cli_signin_changed`, [Check again] and the window regaining focus (never on a timer).
 */
export const cliSigninKey = (provider: string) => ["cli-signin", provider] as const;
/** M4: re-read after a pushed `ServicePush` signal or a user action, never on a timer. */
export const CLIENTS_KEY = ["clients"] as const;
export const SERVICE_CLIENTS_KEY = ["service-clients"] as const;
export const GROUPS_KEY = ["groups"] as const;
export const ACTIVITY_KEY = ["activity"] as const;
export const EGRESS_KEY = ["egress"] as const;
export const FOLDER_DETAIL_KEY = ["folder-detail"] as const;
export const INDEX_LOCATION_KEY = ["index-location"] as const;
export const ADVANCED_KEY = ["advanced"] as const;

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: Infinity, gcTime: Infinity, retry: false, refetchOnWindowFocus: false },
    mutations: { retry: false },
  },
});

export function useAppView(): AppView | undefined {
  return useQuery({ queryKey: VIEW_KEY, queryFn: () => commands.getState() }).data;
}

export function useClaudeStatus(): ConnectStatus | undefined {
  return useQuery({ queryKey: CLAUDE_KEY, queryFn: () => commands.getClaudeStatus() }).data;
}

/** Subscribe once (root layout) to the pushed events and write them into the cache. */
export function useLiveUpdates() {
  const qc = useQueryClient();
  useEffect(() => {
    const offs = [
      events.stateChanged.listen((e) => {
        const before = qc.getQueryData<AppView>(VIEW_KEY);
        qc.setQueryData(VIEW_KEY, e.payload);
        // A pushed provider health change re-reads the provider status once (event-driven).
        if (before && before.provider_health !== e.payload.provider_health) {
          void qc.invalidateQueries({ queryKey: PROVIDER_KEY });
        }
        // The service restarted (update, [Restart now], index move): what it serves may have
        // changed, so the open page reads once again.
        if (before && before.reconnects !== e.payload.reconnects) {
          for (const k of [SERVICE_CLIENTS_KEY, CLIENTS_KEY, GROUPS_KEY, FOLDER_DETAIL_KEY, INDEX_LOCATION_KEY]) {
            void qc.invalidateQueries({ queryKey: k });
          }
        }
      }),
      events.claudeConfigChanged.listen((e) => qc.setQueryData(CLAUDE_KEY, e.payload)),
      events.clientsChanged.listen((e) => {
        // Keep "Last used" from the last read; the file states are fresh.
        const prev = qc.getQueryData<ClientsView>(CLIENTS_KEY);
        const apps = e.payload.apps.map((a) => ({
          ...a,
          last_used_ms: a.last_used_ms ?? prev?.apps.find((p) => p.app === a.app)?.last_used_ms ?? null,
        }));
        qc.setQueryData(CLIENTS_KEY, { ...e.payload, apps });
      }),
      events.servicePush.listen((e) => {
        const s = e.payload;
        switch (s.kind) {
          case "groups_changed":
            void qc.invalidateQueries({ queryKey: GROUPS_KEY });
            void qc.invalidateQueries({ queryKey: FOLDER_DETAIL_KEY });
            break;
          case "clients_changed":
            void qc.invalidateQueries({ queryKey: SERVICE_CLIENTS_KEY });
            void qc.invalidateQueries({ queryKey: CLIENTS_KEY });
            break;
          case "activity_added":
            void qc.invalidateQueries({ queryKey: ACTIVITY_KEY });
            break;
          case "egress_recorded":
            void qc.invalidateQueries({ queryKey: EGRESS_KEY });
            break;
          case "request_progress":
            setRequestLine(s.request_id, s.line);
            break;
          case "index_move_done":
            indexMoveResultStore.set({ kind: "done", path: s.path });
            void qc.invalidateQueries({ queryKey: INDEX_LOCATION_KEY });
            void qc.invalidateQueries({ queryKey: ABOUT_KEY });
            toast.success("Search index moved.");
            break;
          case "index_move_failed":
            indexMoveResultStore.set({ kind: "failed", error: s.error, keptPath: s.kept_path });
            void qc.invalidateQueries({ queryKey: INDEX_LOCATION_KEY });
            break;
          case "cli_signin_changed":
            // Pushed by the service (the tool's sign-in window closed, a check found a new
            // state); no token exists.
            qc.setQueryData(cliSigninKey(s.status.provider), s.status);
            void qc.invalidateQueries({ queryKey: PROVIDER_KEY });
            break;
        }
      }),
    ];
    return () => {
      for (const off of offs) void off.then((un) => un());
    };
  }, [qc]);
}

/** Unwrap a typed command result; throws the UiError so callers handle one shape. */
export async function unwrap<T>(r: Promise<{ status: "ok"; data: T } | { status: "error"; error: UiError }>): Promise<T> {
  const v = await r;
  if (v.status === "error") throw v.error;
  return v.data;
}

export function isUiError(e: unknown): e is UiError {
  return typeof e === "object" && e !== null && "title" in e && "action" in e;
}

/** Any thrown value as a UiError (unknown failures get the plain "Something went wrong"). */
export function asUiError(e: unknown): UiError {
  if (isUiError(e)) return e;
  const wire = fromWire(e);
  if (wire) return wire;
  return {
    title: "Something went wrong",
    body: "An unexpected error happened.",
    action: { kind: "copy_diagnostics" },
    action_label: "Copy diagnostics",
    details: String(e),
  };
}

export async function copyText(text: string, done: string) {
  try {
    await navigator.clipboard.writeText(text);
    toast.success(done);
  } catch {
    toast.error("Couldn't copy", { description: `${OS_SUBJECT} didn't allow access to the clipboard. Try again.` });
  }
}

export async function copyDiagnostics() {
  try {
    const text = await unwrap(commands.getDiagnostics());
    await copyText(text, "Diagnostics copied. Paste them into your message to support.");
  } catch (e) {
    toast.error(asUiError(e).title, { description: asUiError(e).body });
  }
}

export async function copySnippet(snippet: string) {
  await copyText(
    snippet,
    "Setup text copied. Paste it into your AI app's MCP settings, then restart that app.",
  );
}

/**
 * [Restart now] / [Restart Selene Retrieval] (page 03, M4): a real restart of the background
 * service. The shell shows "Restarting the background service…" until it is back (pushed).
 */
export async function restartService() {
  try {
    await unwrap(commands.restartService());
  } catch (e) {
    const err = asUiError(e);
    // P1.5 / M4 tester P2: a failed restart says what happened and offers one next action, and
    // stays until dismissed so keyboard users can reach the button (WCAG 2.2.1).
    // #40 ConnectionNotTrusted (and policy locks): trying again can't help, so no button; the
    // body already says what to do ("Restart Windows to fix this.").
    const action = !canRetry(err)
      ? undefined
      : err.action.kind === "copy_diagnostics"
        ? { label: err.action_label, onClick: () => void copyDiagnostics() }
        : {
            label: err.action.kind === "restart_app" && err.action_label ? err.action_label : "Retry",
            onClick: () => void restartService(),
          };
    toast.error(err.title, { description: err.body, action, duration: Infinity });
  }
}

/** "Saved · This takes effect the next time Selene Retrieval starts." + [Restart now] (OQ-24). */
export function toastRestartNeeded() {
  toast.info("Saved", {
    description: "This takes effect the next time Selene Retrieval starts.",
    action: { label: "Restart now", onClick: () => void restartService() },
    duration: Infinity,
  });
}

/**
 * Performs the one next action attached to an error (01-flows §2). `retry` is the caller's retry.
 * Inside the wizard, pages pass their own handler for folder actions (reopen the picker there).
 */
export function useRunAction() {
  const navigate = useNavigate();
  const qc = useQueryClient();
  return (err: UiError, retry?: () => void) => {
    const a = err.action;
    switch (a.kind) {
      case "retry":
      case "wait_and_retry":
        retry?.();
        break;
      case "choose_folder":
      case "choose_subfolder":
        // OQ-16: open the picker in place (page 09 add flow on the Overview), never the wizard,
        // so an existing user's "Start with Windows" choice is never re-applied.
        void navigate({ to: "/" });
        startAddFolder(a.kind === "choose_subfolder" ? a.path : undefined);
        break;
      case "open_settings":
        void navigate({ to: "/settings" });
        break;
      case "add_provider":
      case "open_providers":
        void navigate({ to: "/providers" });
        break;
      case "reenter_key": {
        // A key field on screen (wizard step 3, the add/replace dialog): focus it, select its text.
        const field = keyFieldStore.get();
        if (field && field.isConnected) {
          field.focus();
          field.select();
        } else {
          void navigate({ to: "/providers" });
          requestReenterKey();
        }
        break;
      }
      case "allow_remote":
        askConsent({ purpose: a.purpose, providerName: a.provider, onDone: (ok) => ok && retry?.() });
        break;
      case "review_approval":
        document.getElementById(`approval-${a.id}`)?.focus();
        break;
      case "view_skipped_files":
        if (a.folder_id) void navigate({ to: "/folders/$id", params: { id: a.folder_id }, search: { section: "skipped" } });
        else void navigate({ to: "/folders" });
        break;
      case "open_folder":
        if (a.folder_id) void navigate({ to: "/folders/$id", params: { id: a.folder_id }, search: {} });
        else void navigate({ to: "/folders" });
        break;
      case "open_connected_apps":
        void navigate({ to: "/connected-apps" });
        break;
      case "restart_app":
        // M4: a real restart (drain + start again), not only a re-attach (OQ-15).
        void restartService();
        break;
      case "restart_claude":
        toast.info("Restart Claude", {
          description: QUIT_CLAUDE_HINT,
        });
        break;
      case "copy_diagnostics":
        void copyDiagnostics();
        break;
      case "copy_snippet": {
        const s = qc.getQueryData<ConnectStatus>(CLAUDE_KEY);
        if (s) void copySnippet(s.snippet);
        break;
      }
      case "free_disk_space":
        toast.info("Free up disk space", {
          description: "Delete or move files you no longer need, then indexing continues on its own.",
        });
        break;
      case "none":
        break;
    }
  };
}

/** Error toast with its one action button. */
export function useErrorToast() {
  const run = useRunAction();
  return (e: unknown, retry?: () => void) => {
    const err = asUiError(e);
    const hasAction = err.action.kind !== "none" && err.action_label !== "";
    toast.error(err.title, {
      description: err.body,
      action: hasAction ? { label: err.action_label, onClick: () => run(err, retry) } : undefined,
      // Page 15 [Change]: an error toast with an action stays until dismissed, so keyboard users
      // can reach the button (WCAG 2.2.1). Plain toasts auto-hide.
      duration: hasAction ? Infinity : undefined,
    });
  };
}

/** A provider is configured (D-21: Prompt Enhancer and [Ask] only exist then). */
export function useHasProvider(): boolean {
  const st = useQuery({ queryKey: PROVIDER_KEY, queryFn: () => unwrap(commands.getProviderStatus()) }).data;
  return Boolean(st?.provider);
}

/** A fresh id for an Ask / Prompt Enhancer request (contract §7: `[A-Za-z0-9_-]`, ≤ 64). */
export function newRequestId(): string {
  return `app-${crypto.randomUUID()}`;
}
