// Page 22: nothing leaves the computer without a clear yes for that purpose (U16). Opened by
// turning a purpose on (page 21), by turning answers on (page 23), by a code 26 [Allow] action, or
// when the service asks (Claude's first answer, `AppView.consent_request`).
import { useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import type { ConsentPurpose, ProviderStatus } from "@/bindings";
import { commands } from "@/bindings";
import { Button } from "@/components/ui/button";
import { Modal } from "@/components/ui/dialog";
import { CONSENTS_KEY, PROVIDER_KEY, unwrap, useAppView, useErrorToast } from "@/lib/app";
import { thousands } from "@/lib/format";
import { consentStore, useStore, type ConsentAsk } from "@/lib/store";
import { CloudUpload } from "lucide-react";

/** Title and body per purpose, verbatim from the brief. */
function copy(purpose: ConsentPurpose, provider: string): { title: string; body: string } {
  switch (purpose) {
    case "answer":
      return {
        title: `Send questions and matching file text to ${provider}?`,
        body: `To write an answer, Selene Retrieval sends your question and the parts of your files that match it to ${provider}. Only the matching parts are sent, never whole folders. ${provider}'s own privacy terms apply.`,
      };
    case "enhance":
      return {
        title: `Send your prompt to ${provider}?`,
        body: `To improve a prompt, Selene Retrieval sends it, with short matching parts of your files, to ${provider}.`,
      };
    case "rerank":
      return {
        title: `Send search results to ${provider}?`,
        body: `To sort results better, the top results of each search are sent to ${provider}.`,
      };
    case "embed_doc":
      return {
        title: `The text of your files will be sent to ${provider}`,
        body: `To search with ${provider}'s model, the text of every file in your folders is sent to ${provider}, now and whenever files change. Search on this computer needs nothing sent.`,
      };
    case "embed_query":
      return {
        title: `Send your search text to ${provider}?`,
        body: `Each search you make is sent to ${provider} to find matches.`,
      };
  }
}

export function useProviderStatus(enabled = true) {
  return useQuery({
    queryKey: PROVIDER_KEY,
    queryFn: () => unwrap(commands.getProviderStatus()),
    enabled,
  });
}

function Dialog({ ask, onClose }: { ask: ConsentAsk; onClose: () => void }) {
  const view = useAppView();
  const qc = useQueryClient();
  const showError = useErrorToast();
  const status: ProviderStatus | undefined = useProviderStatus().data;
  const [busy, setBusy] = useState(false);
  const denyRef = useRef<HTMLButtonElement>(null);
  const provider = status?.provider_name ?? ask.providerName ?? "your AI provider";
  const { title, body } = copy(ask.purpose, provider);
  const strong = ask.purpose === "embed_doc";
  const files = (view?.folders ?? []).filter((f) => f.state !== "removing").reduce((n, f) => n + f.files_total, 0);

  const finish = (allowed: boolean) => {
    if (!allowed && ask.fromService) {
      commands.dismissConsentRequest().catch(() => undefined);
      // Tell the service the question was answered, so it doesn't ask again after a reconnect.
      if (status?.provider) commands.setConsent(status.provider, ask.purpose, false).catch(() => undefined);
    }
    ask.onDone?.(allowed);
    onClose();
  };
  const allow = async () => {
    const id = status?.provider;
    if (!id) {
      finish(false);
      return;
    }
    setBusy(true);
    try {
      const rows = await unwrap(commands.setConsent(id, ask.purpose, true));
      qc.setQueryData(CONSENTS_KEY, rows);
      finish(true);
    } catch (e) {
      showError(e, () => void allow());
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal
      open
      alert
      busy={busy}
      onClose={() => finish(false)}
      initialFocus={denyRef}
      title={title}
      icon={<CloudUpload className="mt-0.5 size-6 shrink-0 text-warning-strong" aria-hidden />}
      description={
        <>
          <p>{body}</p>
          {strong && files > 0 && <p className="text-muted-foreground">About {thousands(files)} files</p>}
        </>
      }
      footer={
        <>
          <Button ref={denyRef} variant="ghost" disabled={busy} onClick={() => finish(false)}>
            Don't allow
          </Button>
          {/* The file-text variant has no default button: [Allow] needs a click or Tab + Enter. */}
          <Button variant={strong ? "outline" : "default"} busy={busy} disabled={!status?.provider} onClick={() => void allow()}>
            Allow
          </Button>
        </>
      }
    />
  );
}

/** Mounted once in the shell: shows the dialog for a local ask or a pending service request. */
export function ConsentHost({ inWizard }: { inWizard: boolean }) {
  const ask = useStore(consentStore);
  const view = useAppView();
  const svc = view?.consent_request ?? null;
  const svcKey = svc ? `${svc.provider}/${svc.purpose}` : null;
  // The service request answered here stays hidden until the next push clears it (no flash).
  const [handled, setHandled] = useState<string | null>(null);
  if (svcKey === null && handled !== null) setHandled(null); // derived reset (React docs pattern)
  const fromSvc =
    svc && !inWizard && svcKey !== handled
      ? { purpose: svc.purpose, providerName: svc.provider_name, fromService: true }
      : null;
  const active: ConsentAsk | null = ask ?? fromSvc;
  if (!active) return null;
  return (
    <Dialog
      key={`${active.purpose}-${active.fromService ? "svc" : "local"}`}
      ask={active}
      onClose={() => {
        if (active.fromService) setHandled(svcKey);
        consentStore.set(null);
      }}
    />
  );
}
