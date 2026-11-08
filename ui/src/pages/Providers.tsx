// Page 21 · Providers & Keys: add, check, change or remove the one AI provider (OQ-33) and its key;
// choose the answer model (automatic by default); see and revoke what may be sent (consents). The
// UI never shows more than the key's last 4 characters. Data is read when the page opens and after
// each action, never on a timer.
import { useEffect, useId, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { commands, type ConsentPurpose, type ConsentRow, type ProviderStatus } from "@/bindings";
import { modelName } from "@/lib/format";
import { useProviderStatus } from "@/components/ConsentDialog";
import { ErrorPanel } from "@/components/ErrorPanel";
import { ManagedLine } from "@/components/Managed";
import { CliSignIn } from "@/components/CliSignIn";
import { CheckAndSaveButton, ProviderForm } from "@/components/ProviderForm";
import { Button } from "@/components/ui/button";
import { Card, CardText, CardTitle } from "@/components/ui/card";
import { Modal } from "@/components/ui/dialog";
import { Select } from "@/components/ui/select";
import { SwitchRow } from "@/components/ui/switch-row";
import { ANSWERS_KEY, asUiError, CONSENTS_KEY, PROVIDER_KEY, unwrap, useErrorToast } from "@/lib/app";
import { CLI_SIGNIN_IDS, isCliSignIn, providerLabel, useCliSignIn } from "@/lib/cliSignIn";
import { PATHS, usePolicy } from "@/lib/policy";
import { askConsent, reenterKeyStore, useStore } from "@/lib/store";
import { CheckCircle2, CloudUpload, Loader2, Sparkles } from "lucide-react";

const AUTO = "__automatic__";

/** One row per purpose; the file-text and search-text rows only with remote embeddings. */
function purposeRows(st: ProviderStatus): { purpose: ConsentPurpose; label: string; strong?: boolean }[] {
  const rows: { purpose: ConsentPurpose; label: string; strong?: boolean }[] = [
    { purpose: "answer", label: "Questions and matching file text (for answers)" },
    { purpose: "enhance", label: "Your prompt (for better prompts)" },
  ];
  if (st.remote_rerank) rows.push({ purpose: "rerank", label: "Search results (for re-ranking)" });
  if (st.remote_embeddings) {
    rows.push({ purpose: "embed_doc", label: "The text of your files (for search)", strong: true });
    rows.push({ purpose: "embed_query", label: "Your search text", strong: true });
  }
  return rows;
}

function purposeHelp(purpose: ConsentPurpose, allowed: boolean, provider: string): string {
  switch (purpose) {
    case "embed_doc":
      return "Every file in your folders is sent, now and whenever files change.";
    case "embed_query":
      return "Each search you make is sent to find matches.";
    case "rerank":
      return `The top results of each search are sent to ${provider}.`;
    case "answer":
      return allowed
        ? `Sent to ${provider} when you or a connected app asks for an answer.`
        : "Not allowed yet. You'll be asked the first time it's needed.";
    case "enhance":
      return allowed
        ? `Sent to ${provider} when you improve a prompt in Selene Retrieval.`
        : "Not allowed yet. You'll be asked the first time it's needed.";
  }
}

function AllowedToSend({ st }: { st: ProviderStatus }) {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const ids = useId();
  const consents = useQuery({ queryKey: CONSENTS_KEY, queryFn: () => unwrap(commands.getConsents()) });
  const provider = st.provider_name ?? "your AI provider";
  const refresh = () => void qc.invalidateQueries({ queryKey: CONSENTS_KEY });

  const toggle = async (purpose: ConsentPurpose, on: boolean) => {
    if (on) {
      // Turning a purpose on always goes through the consent dialog (page 22).
      askConsent({ purpose, providerName: provider, onDone: refresh });
      return;
    }
    try {
      const rows: ConsentRow[] = await unwrap(commands.setConsent(st.provider ?? "", purpose, false));
      qc.setQueryData(CONSENTS_KEY, rows);
      if (purpose === "answer") void qc.invalidateQueries({ queryKey: ANSWERS_KEY });
    } catch (e) {
      showError(e, () => void toggle(purpose, on));
    }
  };

  return (
    <Card className="space-y-4" role="region" aria-labelledby={`${ids}-h`}>
      <CardTitle id={`${ids}-h`}>Allowed to send</CardTitle>
      {consents.error && <ErrorPanel error={asUiError(consents.error)} onRetry={() => void consents.refetch()} />}
      {consents.data &&
        purposeRows(st).map((r) => {
          const allowed = consents.data.find((c) => c.purpose === r.purpose)?.allowed ?? false;
          return (
            <SwitchRow
              key={r.purpose}
              label={
                <span className="inline-flex items-center gap-1.5">
                  {r.strong && <CloudUpload className="size-4 shrink-0 text-warning-strong" aria-hidden />}
                  {r.label}
                </span>
              }
              help={purposeHelp(r.purpose, allowed, provider)}
              checked={allowed}
              onChange={(v) => void toggle(r.purpose, v)}
            />
          );
        })}
    </Card>
  );
}

function ProviderRows({ st, onReplace }: { st: ProviderStatus; onReplace: () => void }) {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const ids = useId();
  const [checking, setChecking] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [removing, setRemoving] = useState(false);
  const keepRef = useRef<HTMLButtonElement>(null);
  const name = st.provider_name ?? st.provider ?? "";

  useEffect(() => {
    if (confirm) keepRef.current?.focus();
  }, [confirm]);

  const apply = (next: ProviderStatus) => {
    qc.setQueryData(PROVIDER_KEY, next);
  };
  const checkAgain = async () => {
    setChecking(true);
    try {
      apply(await unwrap(commands.checkProvider()));
    } catch (e) {
      showError(e, () => void checkAgain());
    } finally {
      setChecking(false);
    }
  };
  const setModel = async (v: string) => {
    try {
      apply(await unwrap(commands.setProviderModel(v === AUTO ? null : v)));
    } catch (e) {
      showError(e, () => void setModel(v));
    }
  };
  const remove = async () => {
    setRemoving(true);
    try {
      apply(await unwrap(commands.removeProvider()));
      qc.removeQueries({ queryKey: CONSENTS_KEY });
      qc.removeQueries({ queryKey: ANSWERS_KEY });
      setConfirm(false);
    } catch (e) {
      showError(e, () => void remove());
    } finally {
      setRemoving(false);
    }
  };

  const local = st.address !== null && !st.key_saved;
  const signIn = isCliSignIn(st.provider ? { id: st.provider } : null);
  const modelOptions = [
    { value: AUTO, label: st.default_model ? `Automatic (${modelName(st.models, st.default_model)})` : "Automatic" },
    ...st.models.map((m) => ({ value: m.id, label: m.name })),
    // A model saved earlier that the list no longer offers stays shown and chosen.
    ...(st.model && !st.models.some((m) => m.id === st.model) ? [{ value: st.model, label: st.model }] : []),
  ];

  return (
    <Card className="space-y-4" role="region" aria-labelledby={`${ids}-h`}>
      <CardTitle id={`${ids}-h`}>AI provider</CardTitle>
      <dl className="grid grid-cols-[minmax(8rem,auto)_1fr] items-center gap-x-6 gap-y-3 text-sm">
        <dt className="text-muted-foreground">Provider</dt>
        <dd>{name}</dd>

        {signIn ? (
          <>
            {/* "Claude (sign in)" / "Codex (sign in)": the tool's own sign-in instead of a key. */}
            <dt className="text-muted-foreground">Account</dt>
            <dd>
              <CliSignIn provider={st.provider ?? ""} signedInAction={false} />
            </dd>
          </>
        ) : local ? (
          <>
            <dt className="text-muted-foreground">Address</dt>
            <dd className="break-all">{st.address}</dd>
          </>
        ) : (
          <>
            <dt className="text-muted-foreground">Key</dt>
            <dd className="flex flex-wrap items-center gap-3">
              {/* Never more than the last 4 characters, also for assistive tech. */}
              <span>{st.key_saved ? `Saved · ends in ${st.key_last4}` : "Not saved"}</span>
              {!confirm && (
                <Button size="sm" variant="outline" onClick={onReplace}>
                  Replace key
                </Button>
              )}
            </dd>
          </>
        )}

        {!confirm && (
          <>
            <dt className="text-muted-foreground">Status</dt>
            <dd aria-live="polite">
              {checking ? (
                <span className="flex items-center gap-2">
                  <Loader2 className="size-4 animate-spin" aria-hidden /> {signIn ? "Checking your sign-in…" : "Checking your key…"}
                </span>
              ) : st.check === "working" ? (
                <span className="flex items-center gap-2">
                  <CheckCircle2 className="size-5 text-success" aria-hidden /> Working
                </span>
              ) : st.check === "failed" && st.last_error ? (
                st.last_error.title
              ) : (
                "Saved, not checked yet"
              )}
            </dd>

            <dt id={`${ids}-model`} className="text-muted-foreground">
              Model for answers
            </dt>
            <dd>
              <Select
                labelledBy={`${ids}-model`}
                value={st.model ?? AUTO}
                onChange={(v) => void setModel(v)}
                options={modelOptions}
                placeholder="Automatic"
              />
            </dd>
          </>
        )}
      </dl>

      {!confirm && st.check === "failed" && st.last_error && (
        <ErrorPanel error={st.last_error} onRetry={() => void checkAgain()} />
      )}

      {confirm ? (
        <div role="group" aria-label={`Remove ${name}?`} className="flex flex-wrap items-center gap-2">
          <span className="text-sm">Remove {name}? Answers and prompt improvement stop. Search keeps working.</span>
          <Button size="sm" variant="danger" busy={removing} onClick={() => void remove()}>
            Remove
          </Button>
          <Button ref={keepRef} size="sm" variant="ghost" disabled={removing} onClick={() => setConfirm(false)}>
            Keep
          </Button>
        </div>
      ) : (
        <div className="flex flex-wrap gap-2">
          <Button variant="outline" busy={checking} onClick={() => void checkAgain()}>
            Check again
          </Button>
          <Button variant="ghost" onClick={() => setConfirm(true)}>
            Remove provider…
          </Button>
        </div>
      )}
    </Card>
  );
}

/**
 * "Claude (sign in)" / "Codex (sign in)" on Providers & Keys while another provider answers
 * (Amendments 10.2 / 10.3): shown when the tool is on this computer. Once it is signed in,
 * [Use for answers…] switches to it after one inline confirm (one provider at a time, OQ-33).
 * Without a provider the [Add a provider] dialog offers them in the provider list.
 */
function CliSignInCard({ provider, current }: { provider: string; current: ProviderStatus }) {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const ids = useId();
  const signIn = useCliSignIn(provider);
  const [confirm, setConfirm] = useState(false);
  const [saving, setSaving] = useState(false);
  const keepRef = useRef<HTMLButtonElement>(null);
  const currentName = current.provider_name ?? current.provider ?? "your provider";
  const label = providerLabel({ id: provider, name: provider });

  useEffect(() => {
    if (confirm) keepRef.current?.focus();
  }, [confirm]);

  const switchTo = async () => {
    setSaving(true);
    try {
      const r = await unwrap(commands.saveProvider(provider, null, null, true));
      qc.setQueryData(PROVIDER_KEY, r.status);
      qc.removeQueries({ queryKey: CONSENTS_KEY });
      setConfirm(false);
    } catch (e) {
      showError(e, () => void switchTo());
    } finally {
      setSaving(false);
    }
  };

  // Only for a tool that is on this computer (no card for every product the user doesn't have).
  if (!signIn.data || signIn.data.state === "not_installed") return null;
  const tool = signIn.data.tool;

  return (
    <Card className="space-y-3" role="region" aria-labelledby={`${ids}-h`}>
      <CardTitle id={`${ids}-h`}>{label}</CardTitle>
      <CardText>Use your {tool} sign-in for answers instead of an API key.</CardText>
      <CliSignIn provider={provider} />
      {signIn.data.signed_in &&
        (confirm ? (
          <div role="group" aria-label={`Use ${label}?`} className="flex flex-wrap items-center gap-2">
            <span className="text-sm">
              Answers and prompt improvement will use {label} instead of {currentName}.
            </span>
            <Button size="sm" busy={saving} onClick={() => void switchTo()}>
              Use {label}
            </Button>
            <Button ref={keepRef} size="sm" variant="ghost" disabled={saving} onClick={() => setConfirm(false)}>
              Keep {currentName}
            </Button>
          </div>
        ) : (
          <Button variant="outline" onClick={() => setConfirm(true)}>
            Use for answers…
          </Button>
        ))}
    </Card>
  );
}

export function ProvidersPage() {
  const status = useProviderStatus();
  const policy = usePolicy();
  // Page 33: cloud providers (or the provider list) may be limited by the organisation; providers
  // that run on this computer stay available, so this is a line, never a dead end.
  const managed = policy.field(PATHS.remoteProviders, PATHS.allowedProviders);
  const remoteManaged = policy.field(PATHS.remoteProviders) !== null;
  const [dialog, setDialog] = useState<"add" | "replace" | null>(null);
  const st = status.data;
  const reenter = useStore(reenterKeyStore);
  const seen = useRef(reenter);

  // "Re-enter key" from an error elsewhere opens the replace dialog once the status is known.
  useEffect(() => {
    if (reenter !== seen.current && st) {
      seen.current = reenter;
      setDialog(st.provider ? "replace" : "add");
    }
  }, [reenter, st]);

  return (
    <div className="space-y-4">
      <h1 className="text-lg font-semibold">Providers &amp; Keys</h1>
      {managed && <ManagedLine field={managed} className="flex items-start gap-2 text-sm text-muted-foreground" />}

      {status.error && <ErrorPanel error={asUiError(status.error)} onRetry={() => void status.refetch()} />}

      {st && !st.provider && (
        <Card className="flex flex-col items-center gap-3 py-10 text-center">
          <Sparkles className="size-8 text-muted-foreground" aria-hidden />
          <CardTitle>No AI provider</CardTitle>
          <CardText>Search works without one. Add one to get written answers and better prompts.</CardText>
          <Button onClick={() => setDialog("add")}>Add a provider</Button>
        </Card>
      )}

      {st?.provider && (
        <>
          <ProviderRows st={st} onReplace={() => setDialog("replace")} />
          <AllowedToSend st={st} />
          {!remoteManaged &&
            CLI_SIGNIN_IDS.filter((id) => id !== st.provider).map((id) => (
              <CliSignInCard key={id} provider={id} current={st} />
            ))}
          {/* Page 21 → page 30 (M4; M3-U deviation 3 closed). */}
          <Link
            to="/activity"
            className="inline-block rounded-sm text-sm text-primary hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
          >
            See what was sent
          </Link>
        </>
      )}

      {dialog && (
        <Modal
          open
          onClose={() => setDialog(null)}
          title={dialog === "replace" && st?.provider_name ? `Replace your ${st.provider_name} key` : "Add an AI provider"}
        >
          <ProviderForm
            fixedProvider={dialog === "replace" ? (st?.provider ?? undefined) : undefined}
            onSaved={() => setDialog(null)}
            actions={(a) => (
              <div className="flex flex-wrap justify-end gap-2 pt-1">
                <Button variant="ghost" disabled={a.checking} onClick={() => setDialog(null)}>
                  Cancel
                </Button>
                <CheckAndSaveButton a={a} variant="default" />
              </div>
            )}
          />
        </Modal>
      )}
    </div>
  );
}
