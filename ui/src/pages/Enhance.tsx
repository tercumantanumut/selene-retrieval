// Page 28 · Prompt Enhancer: turn a rough prompt into a better one with context from the user's
// folders. Only the prompt and the scope are sent (OQ-43). Reachable from "More" only once a
// provider exists (D-21); a direct visit without one shows the empty state.
import { useId, useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { commands, type ConsentRow, type EnhanceView, type UiError } from "@/bindings";
import { useProviderStatus } from "@/components/ConsentDialog";
import { EmptyState } from "@/components/EmptyState";
import { ErrorPanel } from "@/components/ErrorPanel";
import { ScopeMenu } from "@/components/ScopeMenu";
import { Button } from "@/components/ui/button";
import { Card, CardTitle } from "@/components/ui/card";
import { Progress } from "@/components/ui/progress";
import { asUiError, CONSENTS_KEY, copyText, newRequestId, queryClient, unwrap, useAppView } from "@/lib/app";
import { thousands } from "@/lib/format";
import { askConsent, requestLineStore, setRequestLine, useStore } from "@/lib/store";
import { ClipboardCopy, Info, Sparkles, Square } from "lucide-react";

type State =
  | { kind: "idle" }
  | { kind: "working"; id: string }
  | { kind: "done"; result: EnhanceView }
  | { kind: "stopped" }
  | { kind: "error"; error: UiError };

export function EnhancePage() {
  const view = useAppView();
  const navigate = useNavigate();
  const provider = useProviderStatus();
  const lines = useStore(requestLineStore);
  const ids = useId();
  const [text, setText] = useState("");
  const [scope, setScope] = useState<string[] | null>(null);
  const [state, setState] = useState<State>({ kind: "idle" });

  if (provider.data && !provider.data.provider) {
    return (
      <div className="space-y-4">
        <h1 className="text-lg font-semibold">Prompt Enhancer</h1>
        <Card>
          <EmptyState
            icon={Sparkles}
            title="Add an AI provider to improve prompts."
            action={<Button onClick={() => void navigate({ to: "/providers" })}>Add a provider</Button>}
          />
        </Card>
      </div>
    );
  }

  const providerName = provider.data?.provider_name ?? "your AI provider";

  const improve = async () => {
    if (!text.trim()) return;
    const id = newRequestId();
    setState({ kind: "working", id });
    try {
      const result = await unwrap(commands.enhancePrompt(text, scope, id));
      setState((s) => (s.kind === "working" && s.id === id ? { kind: "done", result } : s));
    } catch (e) {
      const err = asUiError(e);
      setState((s) =>
        s.kind === "working" && s.id === id
          ? err.details.startsWith("Cancelled")
            ? { kind: "stopped" }
            : { kind: "error", error: err }
          : s,
      );
    } finally {
      setRequestLine(id, null);
    }
  };
  /** The consent dialog ("Send your prompt to {provider}?") opens first when not allowed yet. */
  const onImprove = async () => {
    let rows: ConsentRow[] | undefined;
    try {
      rows = await queryClient.fetchQuery({ queryKey: CONSENTS_KEY, queryFn: () => unwrap(commands.getConsents()) });
    } catch {
      rows = undefined;
    }
    if (rows && !(rows.find((r) => r.purpose === "enhance")?.allowed ?? false)) {
      askConsent({ purpose: "enhance", onDone: () => void improve() });
      return;
    }
    void improve();
  };
  const stop = () => {
    if (state.kind !== "working") return;
    const id = state.id;
    setState({ kind: "stopped" });
    unwrap(commands.cancelRequest(id)).catch(() => undefined);
  };

  const working = state.kind === "working";
  const line = working ? (lines[state.id] ?? "Improving your prompt…") : "";

  return (
    <div className="space-y-4">
      <h1 className="text-lg font-semibold">Prompt Enhancer</h1>
      <p className="text-sm text-muted-foreground">
        Write what you want to ask. Selene Retrieval adds useful context from your folders and improves the wording.
      </p>
      <Card className="space-y-3">
        <div className="flex items-baseline justify-between gap-3">
          <label htmlFor={`${ids}-p`} className="text-sm font-medium">
            Your prompt
          </label>
          <span id={`${ids}-c`} className="text-xs text-muted-foreground">
            {thousands(text.length)} characters
          </span>
        </div>
        <textarea
          id={`${ids}-p`}
          value={text}
          rows={6}
          aria-describedby={`${ids}-c`}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && e.ctrlKey && text.trim() && !working) {
              e.preventDefault();
              void onImprove();
            }
          }}
          className="field-sizing-content max-h-[24rem] min-h-[9rem] w-full resize-y rounded-md border border-control-off bg-background p-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
        />
        <div className="flex flex-wrap items-center justify-between gap-3">
          <div className="flex flex-wrap gap-2">
            <Button disabled={!text.trim()} busy={working} onClick={() => void onImprove()}>
              <Sparkles /> Improve prompt
            </Button>
            <Button
              variant="ghost"
              onClick={() => {
                setText("");
                if (!working) setState({ kind: "idle" });
              }}
            >
              Clear
            </Button>
          </div>
          <span className="flex items-center gap-2 text-sm text-muted-foreground">
            Use context from:
            <ScopeMenu folders={view?.folders ?? []} value={scope} onChange={setScope} label="Use context from" />
          </span>
        </div>
      </Card>

      {/* Page 28: phase lines and the outcome are announced once (polite); the improved prompt
          itself is read where it is, never pushed through a live region. */}
      <p className="sr-only" role="status" aria-live="polite" aria-atomic="true">
        {working ? line : state.kind === "done" ? "Improved prompt ready." : state.kind === "stopped" ? "Stopped." : ""}
      </p>
      {working && (
        <Card className="space-y-2" aria-busy="true">
          <p className="text-sm">{line}</p>
          <Progress value={null} label="Prompt Enhancer" valueText={line} />
          <Button size="sm" variant="ghost" onClick={stop}>
            <Square /> Stop
          </Button>
        </Card>
      )}
      {state.kind === "stopped" && (
        <Card className="space-y-2">
          <p className="text-sm">Stopped.</p>
        </Card>
      )}
      {state.kind === "error" && <ErrorPanel error={state.error} onRetry={() => void improve()} />}
      {state.kind === "done" && (
        <Card className="space-y-3" role="region" aria-labelledby={`${ids}-done`}>
          <div className="flex flex-wrap items-center justify-between gap-3">
            <CardTitle id={`${ids}-done`}>Improved prompt</CardTitle>
            <Button size="sm" variant="outline" onClick={() => void copyText(state.result.prompt, "Prompt copied.")}>
              <ClipboardCopy /> Copy
            </Button>
          </div>
          <div
            tabIndex={0}
            role="region"
            aria-label="Improved prompt"
            data-surface
            className="whitespace-pre-wrap break-words rounded-lg bg-muted p-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
          >
            {state.result.prompt}
          </div>
          {state.result.files.length > 0 && (
            <details className="text-sm">
              <summary className="cursor-pointer select-none rounded-sm text-muted-foreground">
                Used context from {thousands(state.result.files.length)} file{state.result.files.length === 1 ? "" : "s"}
              </summary>
              <ul className="mt-1 list-disc space-y-0.5 pl-5">
                {state.result.files.map((f, i) => (
                  <li key={`${f}-${i}`}>{f}</li>
                ))}
              </ul>
            </details>
          )}
          {state.result.fallback && (
            <p className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <Info className="size-3.5 shrink-0" aria-hidden />
              Improved without AI this time ({state.result.provider_name ?? providerName} didn't respond).
            </p>
          )}
        </Card>
      )}
    </div>
  );
}
