// Page 26 · Search and page 27 · Answer ("Ask"). Searches only on Enter / [Search] (each search
// is a user action, never as-you-type and never on a timer). Snippets and answers are untrusted
// text: rendered as React text nodes, never as markup (02 §2.6).
import { useEffect, useId, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { commands, type AnswerView, type ConsentRow, type ResultFile, type SearchView, type SourceView, type UiError } from "@/bindings";
import { useProviderStatus } from "@/components/ConsentDialog";
import { EmptyState } from "@/components/EmptyState";
import { ErrorPanel } from "@/components/ErrorPanel";
import { ScopeMenu } from "@/components/ScopeMenu";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { Progress } from "@/components/ui/progress";
import {
  ANSWERS_KEY,
  asUiError,
  CONSENTS_KEY,
  copyText,
  newRequestId,
  unwrap,
  useAppView,
  useErrorToast,
} from "@/lib/app";
import { modelName, thousands } from "@/lib/format";
import { askConsent, focusSearchStore, requestLineStore, setRequestLine, startAddFolder, useStore } from "@/lib/store";
import { ClipboardCopy, FolderOpen, FolderPlus, Search, SearchX, Sparkles, Square } from "lucide-react";
import { FILE_MANAGER } from "@/lib/platform";

/** Results per "page" (page 26 "Show more results" asks for more, up to the engine's 150). */
const PAGE = 50;
const MAX = 150;

/** Recent queries: in memory for this session only, never stored (page 26 privacy). */
const recent: string[] = [];
function remember(q: string) {
  const i = recent.indexOf(q);
  if (i >= 0) recent.splice(i, 1);
  recent.unshift(q);
  recent.length = Math.min(recent.length, 3);
}

function terms(q: string): string[] {
  return q
    .split(/\s+/)
    .map((t) => t.replace(/[^\p{L}\p{N}_-]/gu, ""))
    .filter((t) => t.length >= 2);
}

/** The query terms in `<mark>` (weight + tint, not colour only). Plain text in, text nodes out. */
function Highlighted({ text, words }: { text: string; words: string[] }) {
  if (words.length === 0) return <>{text}</>;
  const esc = words.map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
  const re = new RegExp(`(${esc.join("|")})`, "giu");
  const parts = text.split(re);
  return (
    <>
      {parts.map((p, i) =>
        i % 2 === 1 ? (
          <mark key={i} className="rounded-sm bg-primary/15 px-px font-semibold text-inherit">
            {p}
          </mark>
        ) : (
          p
        ),
      )}
    </>
  );
}

function ResultCard({ file, words, index }: { file: ResultFile; words: string[]; index: number }) {
  const navigate = useNavigate();
  const showError = useErrorToast();
  const [more, setMore] = useState(false);
  const [best, ...rest] = file.hits;
  if (!best) return null;
  const hits = more ? file.hits : [best];
  const showInExplorer = () =>
    unwrap(commands.showInFolder(file.path)).catch((e) => showError(e, () => void showInExplorer()));
  return (
    <li id={`result-${index}`} data-result-path={file.path}>
      <Card className="space-y-2 target-highlight">
        <h2 tabIndex={-1} data-result-heading className="truncate text-base font-semibold focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background" title={file.file_name}>
          {file.file_name}
        </h2>
        <p className="truncate text-xs text-muted-foreground" title={file.rel_path}>
          {file.folder_name} · {file.rel_path.replace(/\//g, "\\")}
        </p>
        {hits.map((h, i) => (
          <div key={`${h.line_start}-${i}`} className="space-y-1">
            <p className="line-clamp-6 whitespace-pre-wrap break-words text-sm">
              <Highlighted text={h.snippet} words={words} />
            </p>
            <p className="text-xs text-muted-foreground">
              Lines {h.line_start}–{h.line_end}
            </p>
          </div>
        ))}
        {rest.length > 0 && !more && (
          <Button variant="link" size="sm" onClick={() => setMore(true)}>
            {rest.length} more {rest.length === 1 ? "match" : "matches"} in this file
          </Button>
        )}
        <div className="flex flex-wrap gap-2">
          <Button size="sm" variant="ghost" onClick={() => void copyText(file.path, "Path copied.")}>
            <ClipboardCopy /> Copy path
          </Button>
          <Button size="sm" variant="ghost" onClick={() => void showInExplorer()}>
            <FolderOpen /> Show in {FILE_MANAGER}
          </Button>
          <Button
            size="sm"
            variant="ghost"
            onClick={() => void navigate({ to: "/folders/$id", params: { id: file.folder_id }, search: {} })}
          >
            Show folder
          </Button>
        </div>
        <details className="text-xs text-muted-foreground">
          <summary className="cursor-pointer select-none">Details</summary>
          <code className="mt-1 block">
            {[
              best.scores.rrf !== null ? `rrf ${best.scores.rrf.toFixed(3)}` : null,
              best.scores.dense !== null ? `dense ${best.scores.dense.toFixed(2)}` : null,
              best.scores.bm25 !== null ? `bm25 ${best.scores.bm25.toFixed(1)}` : null,
              best.scores.rerank !== null ? `rerank ${best.scores.rerank.toFixed(2)}` : null,
            ]
              .filter(Boolean)
              .join(" · ")}
          </code>
        </details>
      </Card>
    </li>
  );
}

/** Jump to the result card for a cited file (or the Sources row), and highlight it (CSS only). */
function jumpTo(path: string, n: number) {
  const card = document.querySelector<HTMLElement>(`[data-result-path="${CSS.escape(path)}"]`);
  const target = card?.querySelector<HTMLElement>("[data-result-heading]") ?? document.getElementById(`source-${n}`);
  if (!target) return;
  target.scrollIntoView({ block: "center" });
  target.focus();
  const box = card?.querySelector<HTMLElement>(".target-highlight");
  if (box) {
    // Restart the 2 s highlight animation (CSS; off under reduced motion). No timer.
    box.classList.remove("flash");
    void box.offsetWidth;
    box.classList.add("flash");
  }
}

/** The answer text with `[n]` citation chips (only for sources the answer really cites). */
function AnswerText({ answer }: { answer: AnswerView }) {
  const byN = new Map(answer.sources.map((s) => [s.n, s]));
  const paragraphs = answer.text.split(/\n{2,}/);
  return (
    <div className="max-w-[70ch] space-y-3 text-[15px] leading-relaxed">
      {paragraphs.map((p, i) => {
        const parts: ReactNode[] = [];
        let last = 0;
        for (const m of p.matchAll(/\[(\d+)\]/g)) {
          const s = byN.get(Number(m[1]));
          if (!s || m.index === undefined) continue;
          parts.push(p.slice(last, m.index));
          parts.push(<Chip key={`${i}-${m.index}`} source={s} />);
          last = m.index + m[0].length;
        }
        parts.push(p.slice(last));
        return (
          <p key={i} className="whitespace-pre-line">
            {parts}
          </p>
        );
      })}
    </div>
  );
}

function Chip({ source: s }: { source: SourceView }) {
  return (
    <button
      type="button"
      onClick={() => jumpTo(s.path, s.n)}
      aria-label={`Source ${s.n}: ${s.file_name}, lines ${s.line_start} to ${s.line_end}`}
      className="mx-0.5 inline-flex h-6 min-w-6 items-center justify-center rounded-md bg-muted px-1.5 align-[1px] text-xs font-medium text-primary hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
    >
      {s.n}
    </button>
  );
}

type AskState =
  | { kind: "idle" }
  | { kind: "working"; id: string; question: string }
  | { kind: "done"; question: string; answer: AnswerView }
  | { kind: "stopped"; question: string }
  | { kind: "error"; question: string; error: UiError };

function AnswerPanel({
  state,
  onStop,
  onAskAgain,
  providerLine,
  deeper,
}: {
  state: AskState;
  onStop: () => void;
  onAskAgain: () => void;
  providerLine: string | null;
  deeper: boolean | null;
}) {
  const lines = useStore(requestLineStore);
  const ids = useId();
  if (state.kind === "idle") return null;
  const a = state.kind === "done" ? state.answer : null;
  const header = a?.provider_name ? [a.provider_name, a.model].filter(Boolean).join(" · ") : providerLine;
  const copyAnswer = () => {
    if (!a) return;
    const src = a.sources
      .map((s) => `[${s.n}] ${s.file_name} · ${s.folder_name} · lines ${s.line_start}–${s.line_end}`)
      .join("\n");
    void copyText(src ? `${a.text}\n\nSources\n${src}` : a.text, "Answer copied.");
  };
  return (
    <Card className="space-y-3" role="region" aria-labelledby={`${ids}-h`} aria-busy={state.kind === "working" || undefined}>
      {/* Page 27: only the phase lines, "Stopped." and "Answer ready" are announced (polite),
          never the answer text itself, which is read where it is. */}
      <p className="sr-only" role="status" aria-live="polite" aria-atomic="true">
        {state.kind === "working"
          ? (lines[state.id] ?? "Reading your files…")
          : state.kind === "stopped"
            ? "Stopped."
            : state.kind === "done"
              ? a && (a.no_good_match || a.no_folders)
                ? `Nothing closely matched “${state.question}” in your folders, so there's nothing to answer from.`
                : "Answer ready"
              : ""}
      </p>
      <div className="flex flex-wrap items-baseline gap-2">
        <Sparkles className="size-4 self-center text-primary" aria-hidden />
        <h2 id={`${ids}-h`} className="text-base font-semibold">
          Answer
        </h2>
        {header && <span className="text-xs text-muted-foreground">{header}</span>}
      </div>
      <div>
        {state.kind === "working" && (
          <div className="space-y-2">
            <p id={`${ids}-w`} className="text-sm">
              {lines[state.id] ?? "Reading your files…"}
            </p>
            <Progress value={null} label="Answer" valueText={lines[state.id] ?? "Reading your files…"} />
            <Button size="sm" variant="ghost" onClick={onStop}>
              <Square /> Stop
            </Button>
          </div>
        )}
        {state.kind === "stopped" && (
          <div className="space-y-2">
            <p className="text-sm">Stopped.</p>
            <Button size="sm" variant="outline" onClick={onAskAgain}>
              Ask again
            </Button>
          </div>
        )}
        {state.kind === "error" && <ErrorPanel error={state.error} onRetry={onAskAgain} />}
        {a && (a.no_good_match || a.no_folders) && (
          <p className="text-sm">
            Nothing closely matched “{state.question}” in your folders, so there's nothing to answer from.
          </p>
        )}
        {a && !a.no_good_match && !a.no_folders && (
          <div className="space-y-4">
            <AnswerText answer={a} />
            {a.sources.length > 0 && (
              <div className="space-y-1.5">
                <h3 className="text-sm font-semibold">Sources</h3>
                <ol className="space-y-1 text-sm">
                  {a.sources.map((s) => (
                    <li key={s.n} id={`source-${s.n}`} tabIndex={-1} className="flex flex-wrap items-baseline gap-1.5 focus-visible:outline-none">
                      <span className="inline-flex h-5 min-w-5 items-center justify-center rounded bg-muted px-1 text-xs font-medium">
                        {s.n}
                      </span>
                      <button
                        type="button"
                        className="rounded-sm text-primary hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
                        onClick={() => jumpTo(s.path, s.n)}
                      >
                        {s.file_name}
                      </button>
                      <span className="text-muted-foreground">
                        · {s.folder_name} · lines {s.line_start}–{s.line_end}
                      </span>
                    </li>
                  ))}
                </ol>
              </div>
            )}
            <div className="flex flex-wrap items-center gap-3">
              <Button size="sm" variant="ghost" onClick={copyAnswer}>
                <ClipboardCopy /> Copy answer
              </Button>
              {deeper !== null && (
                // M4 tester P2: lands on the Answers card, not the top of Settings.
                <Link
                  to="/settings"
                  search={{ section: "answers" }}
                  className="rounded-sm text-sm text-primary underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
                >
                  Deeper answers is {deeper ? "on" : "off"}
                </Link>
              )}
            </div>
            <p className="text-xs text-muted-foreground">
              Written by {a.provider_name ?? "your AI provider"} from your files. Check the sources.
            </p>
          </div>
        )}
      </div>
    </Card>
  );
}

export function SearchPage() {
  const initial = useSearch({ from: "/search" }).q ?? "";
  const view = useAppView();
  const qc = useQueryClient();
  const navigate = useNavigate();
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLOListElement>(null);
  const focusReq = useStore(focusSearchStore);
  const [q, setQ] = useState(initial);
  const [scope, setScope] = useState<string[] | null>(null);
  const [result, setResult] = useState<{ query: string; view: SearchView } | null>(null);
  const [error, setError] = useState<UiError | null>(null);
  const [searching, setSearching] = useState(false);
  const [ask, setAsk] = useState<AskState>({ kind: "idle" });

  const provider = useProviderStatus();
  const hasProvider = Boolean(provider.data?.provider);
  const answers = useQuery({
    queryKey: ANSWERS_KEY,
    queryFn: () => unwrap(commands.getAnswersSettings()),
    enabled: hasProvider,
  }).data;
  // [Ask] exists only with a provider and answers on (D-21, page 27).
  const canAsk = hasProvider && (answers?.answers_enabled ?? false);
  const providerLine = provider.data?.provider_name
    ? [provider.data.provider_name, modelName(provider.data.models, provider.data.model ?? provider.data.default_model)]
        .filter(Boolean)
        .join(" · ")
    : null;

  useEffect(() => {
    inputRef.current?.focus();
  }, [focusReq]);

  const run = async (query: string, max = PAGE) => {
    const text = query.trim();
    if (!text) return;
    setSearching(true);
    setError(null);
    try {
      const v = await unwrap(commands.search(text, scope, max));
      setResult({ query: text, view: v });
      remember(text);
    } catch (e) {
      setError(asUiError(e));
    } finally {
      setSearching(false);
    }
  };

  // The Overview search box lands here with its query: one search (the user's submit), then it's
  // the user's page. Not cached, never repeated on its own.
  const handoff = useQuery({
    queryKey: ["search-handoff", initial],
    queryFn: async () => {
      const v = await unwrap(commands.search(initial, null, PAGE));
      remember(initial);
      return v;
    },
    enabled: Boolean(initial) && result === null && error === null,
    gcTime: 0,
  });

  const doAsk = async (question: string) => {
    const text = question.trim();
    if (!text) return;
    const id = newRequestId();
    setAsk({ kind: "working", id, question: text });
    void run(text);
    try {
      const answer = await unwrap(commands.ask(text, scope, id));
      setAsk((s) => (s.kind === "working" && s.id === id ? { kind: "done", question: text, answer } : s));
    } catch (e) {
      const err = asUiError(e);
      setAsk((s) =>
        s.kind === "working" && s.id === id
          ? err.details.startsWith("Cancelled")
            ? { kind: "stopped", question: text }
            : { kind: "error", question: text, error: err }
          : s,
      );
    } finally {
      setRequestLine(id, null);
    }
  };
  /** The consent dialog opens first when "questions and matching file text" isn't allowed yet. */
  const onAsk = async () => {
    const text = q.trim();
    if (!text) return;
    let rows: ConsentRow[] | undefined;
    try {
      rows = await qc.fetchQuery({ queryKey: CONSENTS_KEY, queryFn: () => unwrap(commands.getConsents()) });
    } catch {
      rows = undefined; // the service answers with its own plain error below
    }
    const allowed = rows?.find((r) => r.purpose === "answer")?.allowed ?? true;
    if (!allowed) {
      // Don't allow → the service's code 26 panel with [Allow] (no provider call is made).
      askConsent({ purpose: "answer", onDone: () => void doAsk(text) });
      return;
    }
    void doAsk(text);
  };
  const onStop = () => {
    if (ask.kind !== "working") return;
    const id = ask.id;
    setAsk({ kind: "stopped", question: ask.question });
    unwrap(commands.cancelRequest(id)).catch(() => undefined);
  };

  const onListKey = (e: KeyboardEvent<HTMLOListElement>) => {
    if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
    const hs = Array.from(e.currentTarget.querySelectorAll<HTMLElement>("[data-result-heading]"));
    const at = hs.findIndex((h) => h.contains(document.activeElement) || h === document.activeElement);
    const cur = at >= 0 ? at : hs.findIndex((h) => h.closest("li")?.contains(document.activeElement));
    if (cur < 0) return;
    e.preventDefault();
    hs[e.key === "ArrowDown" ? Math.min(cur + 1, hs.length - 1) : Math.max(cur - 1, 0)]?.focus();
  };

  const folders = view?.folders ?? [];
  const shownResult = result ?? (initial && handoff.data ? { query: initial, view: handoff.data } : null);
  const shownError = error ?? (result === null && handoff.error ? asUiError(handoff.error) : null);
  const r = shownResult?.view;
  const words = terms(shownResult?.query ?? "");
  const indexing = folders.some((f) => f.state === "waiting" || f.state === "scanning" || f.state === "indexing");

  return (
    <div className="space-y-4">
      <h1 className="text-lg font-semibold">Search</h1>
      <form
        role="search"
        // Sticks right under the header. The offset is in rem (header: 2 × 0.75 rem padding + a
        // 2 rem nav row + 1 px border), so it follows Windows text scaling (R-UX42).
        className="sticky top-[calc(3.5rem+1px)] z-[5] -mx-1 flex flex-wrap gap-2 bg-background px-1 py-1"
        onSubmit={(e) => {
          e.preventDefault();
          void run(q);
        }}
      >
        <label className="relative block min-w-[12rem] flex-1">
          <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" aria-hidden />
          <input
            ref={inputRef}
            value={q}
            onChange={(e) => setQ(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setQ("");
            }}
            placeholder="Search your folders"
            aria-label="Search your folders"
            className="h-11 w-full rounded-md border border-control-off bg-background pl-9 pr-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
          />
        </label>
        <Button type="submit" className="h-11" busy={searching}>
          Search
        </Button>
        {canAsk && (
          <Button type="button" variant="outline" className="h-11" disabled={ask.kind === "working" || !q.trim()} onClick={() => void onAsk()}>
            <Sparkles /> Ask
          </Button>
        )}
        <ScopeMenu folders={folders} value={scope} onChange={setScope} label="Search in" />
      </form>
      {hasProvider && answers && !answers.answers_enabled && (
        // M4 tester P2: [Ask] depends on the answers switch; say where it is (no dead end).
        <p className="text-sm text-muted-foreground">
          Ask is off.{" "}
          <Link
            to="/settings"
            search={{ section: "answers" }}
            className="rounded-sm text-primary underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
          >
            Turn on answers in Settings
          </Link>
        </p>
      )}

      <AnswerPanel
        state={ask}
        onStop={onStop}
        onAskAgain={() => void doAsk(ask.kind === "idle" ? q : ask.question)}
        providerLine={providerLine}
        deeper={answers ? answers.deeper_answers : null}
      />

      {shownError && <ErrorPanel error={shownError} onRetry={() => void run(q)} />}

      {!r && !shownError && !handoff.isFetching && (
        <Card>
          <EmptyState
            icon={Search}
            title="Search your folders"
            text="Find text in your documents and code. Everything stays on this computer."
            action={
              recent.length > 0 ? (
                <div className="flex flex-wrap justify-center gap-2">
                  {recent.map((t) => (
                    <Button
                      key={t}
                      size="sm"
                      variant="outline"
                      onClick={() => {
                        setQ(t);
                        void run(t);
                      }}
                    >
                      {t}
                    </Button>
                  ))}
                </div>
              ) : undefined
            }
          />
        </Card>
      )}

      {r?.no_folders && (
        <Card>
          <EmptyState
            icon={FolderPlus}
            title="Add a folder to search it."
            action={
              <Button
                onClick={() => {
                  void navigate({ to: "/folders" }).then(() => startAddFolder());
                }}
              >
                Add a folder
              </Button>
            }
          />
        </Card>
      )}

      {r && !r.no_folders && (r.no_good_match || r.files.length === 0) && (
        <Card role="status">
          <EmptyState
            icon={SearchX}
            title={`Nothing closely matched “${shownResult?.query}” in your folders.`}
            text="Try other words, or check that the folder with this file is added."
          />
        </Card>
      )}

      {r && r.files.length > 0 && (
        <div className="space-y-3" aria-busy={searching || undefined}>
          <p role="status" className="text-xs text-muted-foreground">
            {thousands(r.files.length)} result{r.files.length === 1 ? "" : "s"} in {thousands(r.folders)} folder
            {r.folders === 1 ? "" : "s"}
            {indexing ? " · Some files are still being indexed" : ""}
            {view?.catching_up ? " · Recent changes may not appear yet." : ""}
          </p>
          <ol ref={listRef} className="space-y-3" aria-label="Results" onKeyDown={onListKey}>
            {r.files.map((f, i) => (
              <ResultCard key={f.path} file={f} words={words} index={i} />
            ))}
          </ol>
          {r.hits >= r.asked && r.asked < MAX && (
            <Button variant="outline" busy={searching} onClick={() => void run(shownResult?.query ?? q, Math.min(MAX, r.asked + PAGE))}>
              Show more results
            </Button>
          )}
        </div>
      )}
    </div>
  );
}
