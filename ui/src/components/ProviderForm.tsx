// The provider + key form shared by wizard step 3 (page 20) and the Add / Replace dialog (page 21).
// Provider names come only from the generated provider list (`getProviders`); nothing is
// hard-coded. The key is sent once to the service ([Check and save] = consent for checking it and
// listing models, D-35), kept in Windows Credential Manager there, and never shown again.
// "Claude (sign in)" / "Codex (sign in)" (Amendments 10.2 / 10.3) take the key field's place with
// the sign-in card: if Claude Code / Codex is signed in on this PC, [Check and save] chooses it
// (no key, no code, nothing to paste).
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { commands, type ProviderOption, type SaveResult, type UiError } from "@/bindings";
import { CliSignIn } from "@/components/CliSignIn";
import { ErrorPanel } from "@/components/ErrorPanel";
import { Button } from "@/components/ui/button";
import { Select } from "@/components/ui/select";
import { asUiError, PROVIDER_KEY, PROVIDERS_LIST_KEY, unwrap, useErrorToast, useRunAction } from "@/lib/app";
import { isCliSignIn, providerLabel, unavailableNote, useCliSignIn } from "@/lib/cliSignIn";
import { MANAGED_TEXT, PATHS, usePolicy } from "@/lib/policy";
import { modelName } from "@/lib/format";
import { keyFieldStore } from "@/lib/store";
import { AlertTriangle, CheckCircle2 } from "lucide-react";

export function useProviderList(): ProviderOption[] | undefined {
  return useQuery({ queryKey: PROVIDERS_LIST_KEY, queryFn: () => commands.getProviders() }).data;
}

type Outcome = { kind: "saved"; result: SaveResult } | { kind: "error"; err: UiError } | null;

export type FormActions = {
  /** [Check and save] (or the key field's Enter). */
  checkAndSave: () => void;
  canCheck: boolean;
  checking: boolean;
  saved: boolean;
  /** A CLI sign-in provider is chosen: there is no key to check. */
  signIn: boolean;
};

export function ProviderForm({
  fixedProvider,
  onSaved,
  onErrorAction,
  actions,
}: {
  /** Replace-key mode: the provider can't be changed. */
  fixedProvider?: string;
  onSaved?: (r: SaveResult) => void;
  /** Override for an error's action (the wizard handles "Open settings" itself). */
  onErrorAction?: (err: UiError, retry: () => void) => boolean;
  /** Where the caller puts its buttons (the wizard: [Skip for now]/[Next] + [Check and save]). */
  actions: (a: FormActions) => ReactNode;
}) {
  const list = useProviderList();
  const qc = useQueryClient();
  const run = useRunAction();
  const policy = usePolicy();
  // Page 33: with cloud providers turned off by policy, only providers on this computer can be
  // chosen; the others stay listed with the reason (never silently missing).
  const remoteOff = policy.field(PATHS.remoteProviders) !== null;
  const blocked = (p: ProviderOption) => remoteOff && p.auth !== "local";
  const ids = useId();
  const [providerId, setProviderId] = useState<string | null>(fixedProvider ?? null);
  const [key, setKey] = useState("");
  const [address, setAddress] = useState<string | null>(null);
  const [show, setShow] = useState(false);
  const [checking, setChecking] = useState(false);
  const [outcome, setOutcome] = useState<Outcome>(null);
  const keyRef = useRef<HTMLInputElement>(null);

  // "Re-enter key" (code 21) focuses this field and selects its text while it is on screen.
  useEffect(() => {
    const el = keyRef.current;
    keyFieldStore.set(el);
    return () => {
      if (keyFieldStore.get() === el) keyFieldStore.set(null);
    };
  });

  const provider = list?.find((p) => p.id === providerId) ?? null;
  const local = provider?.auth === "local";
  const signIn = isCliSignIn(provider);
  const signInStatus = useCliSignIn(provider?.id, signIn);
  const signedIn = signIn && signInStatus.data?.signed_in === true;
  const addressValue = address ?? provider?.default_address ?? "";
  const canCheck =
    provider !== null &&
    provider.available &&
    !blocked(provider) &&
    !checking &&
    (signIn ? signedIn : local ? addressValue.trim() !== "" : key.trim() !== "");

  const save = async (check: boolean) => {
    if (!provider) return;
    setChecking(true);
    setOutcome(null);
    try {
      const r = await unwrap(
        commands.saveProvider(provider.id, local || signIn ? null : key, local ? addressValue : null, check),
      );
      setKey(""); // never kept or shown again
      setShow(false);
      qc.setQueryData(PROVIDER_KEY, r.status);
      setOutcome({ kind: "saved", result: r });
      onSaved?.(r);
    } catch (e) {
      setOutcome({ kind: "error", err: asUiError(e) });
    } finally {
      setChecking(false);
    }
  };
  const checkAndSave = () => {
    if (canCheck) void save(true);
  };
  const onAction = (err: UiError) => {
    const retry = () => void save(true);
    if (onErrorAction?.(err, retry)) return;
    run(err, retry);
  };

  const options = (list ?? []).map((p) => ({
    value: p.id,
    label: providerLabel(p),
    disabled: !p.available || blocked(p),
    note: blocked(p) ? MANAGED_TEXT : p.available ? undefined : unavailableNote(p),
  }));
  const name = provider ? providerLabel(provider) : undefined;
  const saved = outcome?.kind === "saved";
  const st = saved ? outcome.result.status : null;
  const model = st ? modelName(st.models, st.model ?? st.default_model) : null;

  return (
    <div className="space-y-4">
      {!fixedProvider && (
        <div className="space-y-1.5">
          <span id={`${ids}-provider`} className="block text-sm font-medium">
            AI provider
          </span>
          <Select
            labelledBy={`${ids}-provider`}
            value={providerId}
            onChange={(v) => {
              setProviderId(v);
              setAddress(null);
              setOutcome(null);
            }}
            options={options}
            placeholder="Choose a provider"
          />
        </div>
      )}

      {signIn ? (
        <div className="space-y-1.5" role="group" aria-label={name}>
          {provider && <CliSignIn provider={provider.id} disabled={blocked(provider)} />}
          <p className="text-xs text-muted-foreground">
            If {signInStatus.data?.tool ?? "the app"} is signed in on this PC, Selene Retrieval uses that sign-in. It
            never sees or keeps your password.
          </p>
        </div>
      ) : local ? (
        <div className="space-y-1.5">
          <label htmlFor={`${ids}-address`} className="block text-sm font-medium">
            Address
          </label>
          <input
            id={`${ids}-address`}
            className="h-9 w-full rounded-md border border-control-off bg-background px-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
            value={addressValue}
            autoComplete="off"
            spellCheck={false}
            onChange={(e) => setAddress(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") checkAndSave();
            }}
          />
          {provider?.key_url && <KeyLink url={provider.key_url} label="Where do I get it?" />}
        </div>
      ) : (
        <div className="space-y-1.5">
          <label htmlFor={`${ids}-key`} className="block text-sm font-medium">
            {name ? `${name} API key` : "API key"}
          </label>
          <div className="flex gap-2">
            <input
              ref={keyRef}
              id={`${ids}-key`}
              type={show ? "text" : "password"}
              className="h-9 min-w-0 flex-1 rounded-md border border-control-off bg-background px-3 font-mono text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:cursor-not-allowed disabled:opacity-50"
              value={key}
              disabled={!provider || !provider.available}
              autoComplete="off"
              autoCorrect="off"
              autoCapitalize="off"
              spellCheck={false}
              aria-describedby={`${ids}-help`}
              onChange={(e) => setKey(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") checkAndSave();
              }}
            />
            <Button
              type="button"
              variant="outline"
              aria-pressed={show}
              disabled={!provider || !provider.available}
              onClick={() => setShow((s) => !s)}
            >
              Show
            </Button>
          </div>
          <p id={`${ids}-help`} className="text-xs text-muted-foreground">
            Your key is checked with {name ?? "your provider"} and kept in Windows Credential Manager, never in a file.
          </p>
          {provider?.key_url && <KeyLink url={provider.key_url} label="Where do I get a key?" />}
        </div>
      )}

      <div aria-live="polite" className="space-y-3">
        {saved && outcome.result.validated && (
          <p className="flex items-center gap-2 text-sm">
            <CheckCircle2 className="size-5 shrink-0 text-success" aria-hidden />
            {signIn
              ? model
                ? `Sign-in works. Answers will use ${model}.`
                : "Sign-in works."
              : model
                ? `Key works. Answers will use ${model}.`
                : "Key works."}
          </p>
        )}
        {saved && !outcome.result.validated && (
          <p className="flex items-center gap-2 text-sm">
            <AlertTriangle className="size-5 shrink-0 text-warning-strong" aria-hidden />
            Key saved. It will be checked when you're online.
          </p>
        )}
        {outcome?.kind === "error" && (
          <ErrorPanel error={outcome.err} onAction={() => onAction(outcome.err)}>
            {outcome.err.details.startsWith("Offline") && !local && !signIn && (
              <div className="space-y-2 text-sm">
                <p>You can save the key now and it will be checked when you're online.</p>
                <Button size="sm" variant="outline" busy={checking} onClick={() => void save(false)}>
                  Save without checking
                </Button>
              </div>
            )}
          </ErrorPanel>
        )}
      </div>

      {actions({ checkAndSave, canCheck, checking, saved, signIn })}
    </div>
  );
}

/**
 * Page 21 / Amendment 8.3: the provider's own page for getting a key (from the provider list,
 * opened in the browser; the app opens only the provider list's own addresses).
 */
function KeyLink({ url, label }: { url: string; label: string }) {
  const showError = useErrorToast();
  const open = () => unwrap(commands.openLink(url)).catch((e) => showError(e, () => void open()));
  return (
    // 24 px tall (WCAG 2.5.8): it stands alone under the field, not inside a sentence.
    <Button type="button" variant="link" size="sm" className="h-6 py-0 text-xs" onClick={() => void open()}>
      {label}
    </Button>
  );
}

/** The [Check and save] button with its busy label. */
export function CheckAndSaveButton({ a, variant = "outline" }: { a: FormActions; variant?: "outline" | "default" }) {
  return (
    <Button variant={variant} disabled={!a.canCheck && !a.checking} busy={a.checking} onClick={a.checkAndSave}>
      {a.checking ? (a.signIn ? "Checking your sign-in…" : "Checking your key…") : "Check and save"}
    </Button>
  );
}
