// "Claude (sign in)" / "Codex (sign in)" (M5-C, Amendments 10.2 / 10.3): helpers shared by the
// wizard step "Get written answers (optional)" (page 20), Providers & Keys (page 21) and the
// provider select. If Claude Code (or Codex) is signed in on this PC, Selene Retrieval uses that
// sign-in; no token ever exists in the app or the service (crates/app `cli_signin.rs`).
//
// The status is read when a card opens, on [Check again], when the app window regains focus
// (the user comes back from the browser or the sign-in window) and when the service pushes
// `cli_signin_changed` (the sign-in window closed). Never on a timer.
import { useEffect } from "react";
import { useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import { commands, type CliSignInStatus, type ProviderOption } from "@/bindings";
import { cliSigninKey, unwrap } from "@/lib/app";

/** The provider-list ids of the CLI sign-in providers. */
export const CLAUDE_SIGNIN_ID = "claudecode";
export const CODEX_SIGNIN_ID = "codex";
export const CLI_SIGNIN_IDS: readonly string[] = [CLAUDE_SIGNIN_ID, CODEX_SIGNIN_ID];

/** Their fixed labels in the provider list (never the tool's own product name). */
const LABELS: Record<string, string> = {
  [CLAUDE_SIGNIN_ID]: "Claude (sign in)",
  [CODEX_SIGNIN_ID]: "Codex (sign in)",
};

export { cliSigninKey };

export function isCliSignIn(p: Pick<ProviderOption, "id"> | null | undefined): boolean {
  return !!p && CLI_SIGNIN_IDS.includes(p.id);
}

/** The label a provider is shown with. */
export function providerLabel(p: Pick<ProviderOption, "id" | "name">): string {
  return LABELS[p.id] ?? p.name;
}

/** Why a provider can't be chosen, as the short note next to it in the provider select. */
export function unavailableNote(p: Pick<ProviderOption, "id" | "disabled_reason">): string {
  if (p.disabled_reason === "pending_vendor_approval") return "pending vendor approval";
  if (p.disabled_reason === "not_allowed_by_vendor") return "not allowed by the vendor";
  return "not available yet";
}

/** One refresh at a time per provider, however many cards listen for focus. */
const inFlight = new Map<string, Promise<CliSignInStatus>>();

/** Asks the tool again now and stores the answer ([Check again], window focus). */
export function recheckCliSignIn(qc: QueryClient, provider: string): Promise<CliSignInStatus> {
  let p = inFlight.get(provider);
  if (!p) {
    p = unwrap(commands.cliSigninStatus(provider, true))
      .then((s) => {
        qc.setQueryData(cliSigninKey(provider), s);
        return s;
      })
      .finally(() => inFlight.delete(provider));
    inFlight.set(provider, p);
  }
  return p;
}

/** The sign-in state of `provider`; re-checked when the app window regains focus. */
export function useCliSignIn(provider: string | null | undefined, enabled = true) {
  const qc = useQueryClient();
  const on = enabled && !!provider && CLI_SIGNIN_IDS.includes(provider);
  const q = useQuery<CliSignInStatus>({
    queryKey: cliSigninKey(provider ?? ""),
    queryFn: () => unwrap(commands.cliSigninStatus(provider ?? "", false)),
    enabled: on,
    // Focus is handled below with a fresh check (the default re-read would reuse the service's
    // cached answer).
    refetchOnWindowFocus: false,
  });
  useEffect(() => {
    if (!on || !provider) return;
    const onFocus = () => {
      recheckCliSignIn(qc, provider).catch(() => {
        // The card keeps its last state; [Check again] shows the error.
      });
    };
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [on, provider, qc]);
  return q;
}
