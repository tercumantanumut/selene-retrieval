// "Claude (sign in)" / "Codex (sign in)" card (M5-C, Amendments 10.2 / 10.3), on the wizard step
// "Get written answers (optional)" (page 20) and Providers & Keys (page 21).
//
// If Claude Code (or Codex) is signed in on this PC, Selene Retrieval uses that sign-in. One
// state, one action:
//   signed in      → "Using your Claude Code sign-in: {email} ({plan})" [Check again]
//   not installed  → "Claude Code isn't installed on this computer" [Get Claude Code]
//   not signed in  → "Claude Code isn't signed in" [Sign in] (the tool's own sign-in window
//                    opens; it opens the browser itself)
//   signing in     → "Finish signing in in the Claude Code window" [Check again]
//   check failed   → "Couldn't check your Claude Code sign-in" [Check again]
// The card updates by itself when the sign-in window closes (pushed) and when this window
// regains focus. No token, code or password is ever asked for or shown.
import { useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { commands, type CliSignInStatus, type UiError } from "@/bindings";
import { ErrorPanel } from "@/components/ErrorPanel";
import { Button } from "@/components/ui/button";
import { announce } from "@/lib/announce";
import { asUiError, unwrap, useErrorToast } from "@/lib/app";
import { cliSigninKey, recheckCliSignIn, useCliSignIn } from "@/lib/cliSignIn";
import { AlertTriangle, CheckCircle2, Loader2 } from "lucide-react";

/** "Using your Claude Code sign-in: me@example.com (Max)". */
export function usingLine(st: CliSignInStatus): string {
  const who = [st.email, st.plan ? `(${st.plan})` : null].filter(Boolean).join(" ");
  return who ? `Using your ${st.tool} sign-in: ${who}` : `Using your ${st.tool} sign-in`;
}

export function CliSignIn({
  provider,
  disabled = false,
  signedInAction = true,
}: {
  provider: string;
  /** Locked by the organisation (page 33): the state is shown, nothing can be started. */
  disabled?: boolean;
  /** Show [Check again] when signed in (off where the page has its own [Check again]). */
  signedInAction?: boolean;
}) {
  const qc = useQueryClient();
  const status = useCliSignIn(provider);
  const showError = useErrorToast();
  const [busy, setBusy] = useState<"start" | "check" | null>(null);
  const [failed, setFailed] = useState<UiError | null>(null);
  const st = status.data;

  const check = async () => {
    setBusy("check");
    setFailed(null);
    try {
      const next = await recheckCliSignIn(qc, provider);
      announce(next.signed_in ? usingLine(next) : "Checked");
    } catch (e) {
      setFailed(asUiError(e));
    } finally {
      setBusy(null);
    }
  };

  const start = async () => {
    setBusy("start");
    setFailed(null);
    try {
      const r = await unwrap(commands.cliSigninStart(provider));
      qc.setQueryData(cliSigninKey(provider), r.status);
      if (r.started) announce(`The ${r.status.tool} sign-in window opened.`);
    } catch (e) {
      setFailed(asUiError(e));
    } finally {
      setBusy(null);
    }
  };

  const openInstall = (url: string) =>
    unwrap(commands.openLink(url)).catch((e) => showError(e, () => void openInstall(url)));

  if (status.error) {
    return <ErrorPanel error={asUiError(status.error)} onRetry={() => void check()} />;
  }
  if (!st) {
    return (
      <p className="flex items-center gap-2 text-sm text-muted-foreground">
        <Loader2 className="size-4 animate-spin" aria-hidden /> Checking your sign-in…
      </p>
    );
  }

  const checkAgain = (
    <Button size="sm" variant="outline" busy={busy === "check"} onClick={() => void check()}>
      Check again
    </Button>
  );

  let line: string;
  let ok = false;
  let action: ReactNode;
  if (st.signed_in) {
    ok = true;
    line = usingLine(st);
    action = signedInAction ? checkAgain : null;
  } else if (st.state === "not_installed") {
    line = `${st.tool} isn't installed on this computer`;
    action = st.install_url ? (
      <Button size="sm" variant="outline" onClick={() => void openInstall(st.install_url ?? "")}>
        Get {st.tool}
      </Button>
    ) : (
      checkAgain
    );
  } else if (st.state === "signing_in") {
    line = `Finish signing in in the ${st.tool} window that opened.`;
    action = checkAgain;
  } else if (st.check_failed) {
    line = `Couldn't check your ${st.tool} sign-in`;
    action = checkAgain;
  } else {
    line = `${st.tool} isn't signed in`;
    action = (
      <Button size="sm" variant="outline" busy={busy === "start"} locked={disabled} onClick={() => void start()}>
        Sign in
      </Button>
    );
  }

  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-3" aria-live="polite">
        <p className="flex items-center gap-2 text-sm">
          {ok ? (
            <CheckCircle2 className="size-5 shrink-0 text-success" aria-hidden />
          ) : (
            <AlertTriangle className="size-4 shrink-0 text-warning-strong" aria-hidden />
          )}
          {line}
        </p>
        {action}
      </div>
      {failed && <ErrorPanel error={failed} onRetry={() => void check()} />}
    </div>
  );
}
