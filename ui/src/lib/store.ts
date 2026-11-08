// Tiny UI-only stores (no timers, no backend reads): which dialog or inline flow is open. Set by
// user actions and error actions; read with `useSyncExternalStore`.
import { useSyncExternalStore } from "react";
import type { ConsentPurpose } from "@/bindings";

export function createStore<T>(initial: T) {
  let value = initial;
  const subs = new Set<() => void>();
  return {
    get: () => value,
    set: (next: T) => {
      value = next;
      for (const s of subs) s();
    },
    subscribe: (cb: () => void) => {
      subs.add(cb);
      return () => {
        subs.delete(cb);
      };
    },
  };
}

export function useStore<T>(store: ReturnType<typeof createStore<T>>): T {
  return useSyncExternalStore(store.subscribe, store.get, store.get);
}

/** The consent dialog (page 22). `onDone(allowed)` continues or stops the original action. */
export type ConsentAsk = {
  purpose: ConsentPurpose;
  /** Display name; when unknown, the configured provider's name is used. */
  providerName?: string;
  /** Asked by the service (Claude's first answer): answering clears its request. */
  fromService?: boolean;
  onDone?: (allowed: boolean) => void;
};
export const consentStore = createStore<ConsentAsk | null>(null);

export function askConsent(ask: ConsentAsk) {
  consentStore.set(ask);
}

/** "Add a folder" flow (page 09), started from Overview or from a `choose_folder` error action. */
export type AddFolderStart = { defaultPath?: string; nonce: number };
export const addFolderStore = createStore<AddFolderStart | null>(null);
let nonce = 0;
export function startAddFolder(defaultPath?: string) {
  nonce += 1;
  addFolderStore.set({ defaultPath, nonce });
}

/** Providers & Keys should open the "Replace your key" dialog (error action `reenter_key`). */
export const reenterKeyStore = createStore<number>(0);
export function requestReenterKey() {
  reenterKeyStore.set(reenterKeyStore.get() + 1);
}

/**
 * Focus target for the wizard's key field: the provider step registers it; `reenter_key` inside
 * the wizard focuses it and selects its text.
 */
export const keyFieldStore = createStore<HTMLInputElement | null>(null);

/**
 * Phase line per running Ask / Prompt Enhancer request ("Reading your files…"), fed by the pushed
 * `request_progress` signal (contract §7). Entries are removed when the request ends.
 */
export const requestLineStore = createStore<Record<string, string>>({});
export function setRequestLine(id: string, line: string | null) {
  const next = { ...requestLineStore.get() };
  if (line === null) delete next[id];
  else next[id] = line;
  requestLineStore.set(next);
}

/** Ctrl+K / Ctrl+F (page 04): the Search page focuses its box when this changes. */
export const focusSearchStore = createStore<number>(0);
export function requestSearchFocus() {
  focusSearchStore.set(focusSearchStore.get() + 1);
}

/** The last "Move the search index" result (pushed `index.move.done` / `index.move.failed`). */
export type IndexMoveResult =
  | { kind: "done"; path: string }
  | { kind: "failed"; error: import("@/bindings").UiError; keptPath: string };
export const indexMoveResultStore = createStore<IndexMoveResult | null>(null);
