// Page 25 · Folder detail: is it up to date, what was skipped and why, and how it's set up. The
// header line and progress are pushed (`AppView.folders`); the detail is read when the page opens
// and again only after a pushed change of this folder's state (never on a timer).
import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams, useSearch } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  commands,
  type ChangeImpact,
  type FieldProblem,
  type FolderDetailView,
  type FolderSettingsView,
  type FolderView,
  type SkippedRow,
} from "@/bindings";
import { EmptyState } from "@/components/EmptyState";
import { ErrorPanel } from "@/components/ErrorPanel";
import { FolderStateIcon, FolderProgress, isBusy, ONLINE_ONLY_NOTE, SEARCH_ITS_CONTENTS, WAS_FILES_ONLY_NOTE } from "@/components/FolderCard";
import { Button } from "@/components/ui/button";
import { Card, CardTitle } from "@/components/ui/card";
import { Disclosure } from "@/components/ui/disclosure";
import { Select } from "@/components/ui/select";
import { SwitchRow } from "@/components/ui/switch-row";
import { asUiError, copyText, FOLDER_DETAIL_KEY, GROUPS_KEY, unwrap, useAppView, useErrorToast } from "@/lib/app";
import { useDefaultFolder } from "@/lib/defaultFolder";
import { thousands } from "@/lib/format";
import { absoluteTime, relativeTime } from "@/lib/time";
import { AlertTriangle, ChevronLeft, ClipboardCopy, Cloud, FolderOpen, FolderX, ListTree, Network, Sparkles, Square } from "lucide-react";

const FILE_TYPES = [
  { value: "documents", label: "Documents" },
  { value: "code", label: "Code" },
  { value: "documents_and_code", label: "Documents and code" },
  { value: "all_text", label: "All text files" },
  { value: "custom", label: "Custom…" },
];
const SYNC_MODES = [
  { value: "auto", label: "As files change, plus a regular full check" },
  { value: "scheduled", label: "On a schedule only" },
  { value: "triggered", label: "As files change only" },
  { value: "manual", label: "Only when I click “Check for changes now”" },
];
const REINDEX = [
  { value: "smart", label: "Smart" },
  { value: "always", label: "Always" },
  { value: "never", label: "Never" },
];
const CHUNKS = [
  { value: "balanced", label: "Balanced" },
  { value: "small", label: "Small" },
  { value: "large", label: "Large" },
  { value: "custom", label: "Custom" },
];
const MB = 1024 * 1024;

/** The size limit in MB for the field: whole numbers, or up to 3 decimals below 1 MB. */
function mbText(bytes: number): string {
  const mb = bytes / MB;
  return mb >= 1 ? String(Math.round(mb)) : String(Number(mb.toFixed(3)));
}

const inputClass =
  "h-9 w-full rounded-md border border-control-off bg-background px-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background aria-[invalid=true]:border-warning-strong";

function FieldError({ id, problem }: { id: string; problem?: FieldProblem }) {
  if (!problem) return null;
  return (
    <p id={id} className="flex items-center gap-1.5 text-xs text-warning-strong">
      <AlertTriangle className="size-3.5 shrink-0" aria-hidden /> {problem.text}
    </p>
  );
}

/** A labelled row (DS-4.18): label above, control, help or error below. */
function Field({ label, help, children, labelId }: { label: string; help?: string; children: ReactNode; labelId?: string }) {
  return (
    <div className="space-y-1.5">
      <span id={labelId} className="block text-sm font-medium">
        {label}
      </span>
      {children}
      {help && <p className="text-xs text-muted-foreground">{help}</p>}
    </div>
  );
}

/** Text that saves on Enter or when the field loses focus. */
function CommitInput({
  value,
  onCommit,
  placeholder,
  ariaLabel,
  errorId,
  invalid,
  inputMode,
  className,
}: {
  value: string;
  onCommit: (v: string) => void;
  placeholder?: string;
  ariaLabel?: string;
  errorId?: string;
  invalid?: boolean;
  inputMode?: "numeric" | "decimal" | "text";
  className?: string;
}) {
  const [text, setText] = useState(value);
  const [seen, setSeen] = useState(value);
  if (value !== seen) {
    // The saved value changed (push or save): show it (derived state, React docs pattern).
    setSeen(value);
    setText(value);
  }
  return (
    <input
      value={text}
      inputMode={inputMode}
      placeholder={placeholder}
      aria-label={ariaLabel}
      aria-invalid={invalid || undefined}
      aria-describedby={invalid ? errorId : undefined}
      className={className ?? inputClass}
      onChange={(e) => setText(e.target.value)}
      onBlur={() => text !== value && onCommit(text)}
      onKeyDown={(e) => {
        if (e.key === "Enter" && text !== value) onCommit(text);
        if (e.key === "Escape") setText(value);
      }}
    />
  );
}

function list(text: string): string[] {
  return text
    .split(/[,\n]/)
    .map((s) => s.trim())
    .filter(Boolean);
}

/** Where a change was made: its confirm is drawn there, and only there (M4 tester P2). */
type Where = "basic" | "advanced";
type Pending = { next: FolderSettingsView; impact: Exclude<ChangeImpact, "none">; where: Where };

function SettingsCards({ detail, onSaved }: { detail: FolderDetailView; onSaved: (d: FolderDetailView) => void }) {
  const showError = useErrorToast();
  const ids = useId();
  const [problems, setProblems] = useState<FieldProblem[]>([]);
  const [pending, setPending] = useState<Pending | null>(null);
  const [saving, setSaving] = useState(false);
  const cancelRef = useRef<HTMLButtonElement>(null);
  const groups = useQuery({ queryKey: GROUPS_KEY, queryFn: () => unwrap(commands.getGroups()) }).data;
  const s = detail.settings;
  const problem = (f: string) => problems.find((p) => p.field === f);

  useEffect(() => {
    if (pending) cancelRef.current?.focus();
  }, [pending]);

  const save = async (next: FolderSettingsView) => {
    setSaving(true);
    try {
      const r = await unwrap(commands.updateFolderSettings(detail.id, next));
      setPending(null);
      setProblems([]);
      onSaved(r.detail);
    } catch (e) {
      showError(e, () => void save(next));
    } finally {
      setSaving(false);
    }
  };
  /** Validate, then save at once or confirm first when the change re-reads the folder. */
  const change = async (patch: Partial<FolderSettingsView>, where: Where) => {
    const next = { ...s, ...patch };
    const found = await commands.checkFolderSettings(next);
    setProblems(found);
    if (found.length > 0) return;
    const impact = await commands.folderChangeImpact(s, next);
    if (impact === "none") void save(next);
    else setPending({ next, impact, where });
  };
  const basic = (patch: Partial<FolderSettingsView>) => void change(patch, "basic");
  const advanced = (patch: Partial<FolderSettingsView>) => void change(patch, "advanced");
  const moveGroup = async (groupId: string) => {
    try {
      await unwrap(commands.moveFolderToGroup(detail.id, groupId));
      toast.success("Saved");
    } catch (e) {
      showError(e, () => void moveGroup(groupId));
    }
  };

  const confirmAt = (where: Where) =>
    pending?.where === where && (
    <div
      role="group"
      aria-label="Re-read the folder?"
      className="flex flex-wrap items-center gap-2 rounded-lg border border-border bg-muted/50 p-3"
      onKeyDown={(e) => {
        if (e.key === "Escape") setPending(null);
      }}
    >
      <p className="text-sm">This re-reads the folder. Search keeps working meanwhile.</p>
      <Button size="sm" busy={saving} onClick={() => void save(pending.next)}>
        Change
      </Button>
      <Button ref={cancelRef} size="sm" variant="ghost" disabled={saving} onClick={() => setPending(null)}>
        Cancel
      </Button>
    </div>
  );
  const onedrive = detail.cloud_note !== null || detail.online_only_skipped > 0 || s.include_online_only;

  return (
    <>
      <Card className="space-y-4">
        <CardTitle>Folder settings</CardTitle>
        <Field label="Name shown in the app and to connected apps">
          <CommitInput
            value={s.display_name}
            placeholder={detail.name}
            ariaLabel="Name shown in the app and to connected apps"
            invalid={Boolean(problem("display_name"))}
            errorId={`${ids}-name`}
            onCommit={(v) => basic({ display_name: v.trim() })}
          />
          <FieldError id={`${ids}-name`} problem={problem("display_name")} />
        </Field>
        <SwitchRow
          label="Keep this folder up to date automatically"
          checked={s.keep_up_to_date}
          onChange={(v) => basic({ keep_up_to_date: v })}
        />
        <Field label="Which kinds of files to include" labelId={`${ids}-types`}>
          <Select
            labelledBy={`${ids}-types`}
            value={s.file_types_preset}
            options={FILE_TYPES}
            placeholder="Documents and code"
            onChange={(v) => basic({ file_types_preset: v as FolderSettingsView["file_types_preset"] })}
          />
        </Field>
        {s.file_types_preset === "custom" && (
          <Field label="Extensions to include when the preset is “custom”" help="For example: md, txt, pdf">
            <CommitInput
              value={s.include_extensions.join(", ")}
              ariaLabel="Extensions to include"
              invalid={Boolean(problem("include_extensions"))}
              errorId={`${ids}-ext`}
              onCommit={(v) => basic({ include_extensions: list(v).map((x) => x.replace(/^\./, "")) })}
            />
            <FieldError id={`${ids}-ext`} problem={problem("include_extensions")} />
          </Field>
        )}
        {onedrive && (
          // Page 25 [M5]: shown for folders with OneDrive files (DS-5.3); off by default.
          <SwitchRow
            label="Include OneDrive files that are only stored online (downloads them)"
            checked={s.include_online_only}
            onChange={(v) => basic({ include_online_only: v })}
          />
        )}
        {confirmAt("basic")}
      </Card>

      <Disclosure label="Advanced for this folder" contentClassName="space-y-4">
          {s.keep_up_to_date && (
            <Field label="When to re-check the folder" labelId={`${ids}-sync`}>
              <Select
                labelledBy={`${ids}-sync`}
                value={s.sync_mode}
                options={SYNC_MODES}
                placeholder="As files change, plus a regular full check"
                onChange={(v) => advanced({ sync_mode: v as FolderSettingsView["sync_mode"] })}
              />
            </Field>
          )}
          <Field label="Full re-check interval in minutes (5-1440)">
            <CommitInput
              value={String(s.cadence_min)}
              inputMode="numeric"
              ariaLabel="Full re-check interval in minutes"
              invalid={Boolean(problem("cadence_min"))}
              errorId={`${ids}-cad`}
              className={`${inputClass} max-w-32`}
              onCommit={(v) => advanced({ cadence_min: Number.parseInt(v, 10) || 0 })}
            />
            <FieldError id={`${ids}-cad`} problem={problem("cadence_min")} />
          </Field>
          <Field label="When to re-index changed files" labelId={`${ids}-re`}>
            <Select
              labelledBy={`${ids}-re`}
              value={s.reindex_policy}
              options={REINDEX}
              placeholder="Smart"
              onChange={(v) => advanced({ reindex_policy: v as FolderSettingsView["reindex_policy"] })}
            />
          </Field>
          <SwitchRow label="Include subfolders" checked={s.recursive} onChange={(v) => advanced({ recursive: v })} />
          <Field label="Extra glob patterns to exclude" help="Separate patterns with commas, for example: **/node_modules/**, *.tmp">
            <CommitInput
              value={s.exclude_globs.join(", ")}
              ariaLabel="Extra glob patterns to exclude"
              invalid={Boolean(problem("exclude_globs"))}
              errorId={`${ids}-glob`}
              onCommit={(v) => advanced({ exclude_globs: list(v) })}
            />
            <FieldError id={`${ids}-glob`} problem={problem("exclude_globs")} />
          </Field>
          {/* M4 tester P2: one unit family (KB / MB, as the field and its error line say). */}
          <Field label="Skip files larger than this many MB" help="From 1 KB (0.001 MB) to 200 MB.">
            <span className="flex items-center gap-2">
              <CommitInput
                value={mbText(s.max_file_bytes)}
                inputMode="decimal"
                ariaLabel="Skip files larger than this many MB"
                invalid={Boolean(problem("max_file_bytes"))}
                errorId={`${ids}-max`}
                className={`${inputClass} max-w-32`}
                onCommit={(v) => advanced({ max_file_bytes: Math.round((Number.parseFloat(v) || 0) * MB) })}
              />
              <span className="text-sm text-muted-foreground">MB</span>
            </span>
            <FieldError id={`${ids}-max`} problem={problem("max_file_bytes")} />
          </Field>
          <Field label="Chunk size preset" labelId={`${ids}-chunk`}>
            <Select
              labelledBy={`${ids}-chunk`}
              value={s.chunk_preset}
              options={CHUNKS}
              placeholder="Balanced"
              onChange={(v) => advanced({ chunk_preset: v as FolderSettingsView["chunk_preset"] })}
            />
          </Field>
          {s.chunk_preset === "custom" && (
            <div className="grid gap-4 sm:grid-cols-2">
              <Field label="Custom chunk size in tokens (64-8192)">
                <CommitInput
                  value={s.chunk_size_override === null ? "" : String(s.chunk_size_override)}
                  inputMode="numeric"
                  ariaLabel="Custom chunk size in tokens"
                  invalid={Boolean(problem("chunk_size_override"))}
                  errorId={`${ids}-cs`}
                  onCommit={(v) => advanced({ chunk_size_override: v.trim() ? Number.parseInt(v, 10) || 0 : null })}
                />
                <FieldError id={`${ids}-cs`} problem={problem("chunk_size_override")} />
              </Field>
              <Field label="Custom chunk overlap in tokens (0-2048)">
                <CommitInput
                  value={s.chunk_overlap_override === null ? "" : String(s.chunk_overlap_override)}
                  inputMode="numeric"
                  ariaLabel="Custom chunk overlap in tokens"
                  invalid={Boolean(problem("chunk_overlap_override"))}
                  errorId={`${ids}-co`}
                  onCommit={(v) =>
                    advanced({ chunk_overlap_override: v.trim() ? Number.parseInt(v, 10) || 0 : null })
                  }
                />
                <FieldError id={`${ids}-co`} problem={problem("chunk_overlap_override")} />
              </Field>
            </div>
          )}
          <SwitchRow
            label="Follow symbolic links"
            help="Never outside the folder."
            checked={s.follow_symlinks}
            onChange={(v) => advanced({ follow_symlinks: v })}
          />
          {groups && groups.length > 0 && (
            <Field label="Folder group" labelId={`${ids}-group`}>
              <Select
                labelledBy={`${ids}-group`}
                value={detail.group_id}
                options={groups.map((g) => ({ value: g.id, label: g.name }))}
                placeholder={detail.group_name}
                onChange={(v) => void moveGroup(v)}
              />
            </Field>
          )}
          {confirmAt("advanced")}
      </Disclosure>
    </>
  );
}

function SkippedCard({ detail, focus }: { detail: FolderDetailView; focus: boolean }) {
  const showError = useErrorToast();
  const headingRef = useRef<HTMLHeadingElement>(null);
  const [all, setAll] = useState<SkippedRow[] | null>(null);
  const [loading, setLoading] = useState(false);
  const rows = all ?? detail.skipped;

  useEffect(() => {
    if (focus) {
      headingRef.current?.scrollIntoView({ block: "start" });
      headingRef.current?.focus();
    }
  }, [focus]);

  const retry = async (ids: string[] | null) => {
    try {
      const n = await unwrap(commands.retryFiles(detail.id, ids));
      toast.success(n === 1 ? "1 file will be read again." : `${thousands(n)} files will be read again.`);
    } catch (e) {
      showError(e, () => void retry(ids));
    }
  };
  const showAll = async () => {
    setLoading(true);
    try {
      const p = await unwrap(commands.getSkippedFiles(detail.id, 0, 500, detail.settings.max_file_bytes));
      setAll(p.files);
    } catch (e) {
      showError(e, () => void showAll());
    } finally {
      setLoading(false);
    }
  };

  return (
    <Card id="skipped" className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 ref={headingRef} tabIndex={-1} className="text-base font-semibold leading-tight focus-visible:outline-none">
          Skipped files ({thousands(detail.skipped_total)})
        </h2>
        {rows.some((r) => r.retryable) && (
          <Button size="sm" variant="outline" onClick={() => void retry(null)}>
            Retry all
          </Button>
        )}
      </div>
      <div className="max-h-[28rem] overflow-auto rounded-md border border-border">
        <table className="w-full table-fixed border-collapse text-sm">
          <caption className="sr-only">Skipped files</caption>
          <thead className="sticky top-0 bg-background">
            <tr className="border-b border-border text-left text-xs text-muted-foreground">
              <th scope="col" className="w-[45%] px-3 py-2 font-medium">File</th>
              <th scope="col" className="px-3 py-2 font-medium">Reason</th>
              <th scope="col" className="w-24 px-3 py-2">
                <span className="sr-only">Action</span>
              </th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.file_id} className="h-10 border-b border-border last:border-b-0">
                <td className="truncate px-3 py-2" title={r.rel_path}>
                  {r.rel_path}
                </td>
                <td className="px-3 py-2">{r.reason}</td>
                <td className="px-3 py-2 text-right">
                  {r.retryable && (
                    <Button size="sm" variant="ghost" onClick={() => void retry([r.file_id])}>
                      Retry
                    </Button>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {all === null && detail.skipped_total > detail.skipped.length && (
        <Button size="sm" variant="outline" busy={loading} onClick={() => void showAll()}>
          Show all
        </Button>
      )}
    </Card>
  );
}

function HeaderCard({
  live,
  detail,
  onSaved,
}: {
  live: FolderView;
  detail: FolderDetailView;
  onSaved: (d: FolderDetailView) => void;
}) {
  const navigate = useNavigate();
  const showError = useErrorToast();
  const [confirm, setConfirm] = useState(false);
  const [sending, setSending] = useState(false);
  const [including, setIncluding] = useState(false);
  const keepRef = useRef<HTMLButtonElement>(null);
  const busy = isBusy(live);
  const removing = live.state === "removing";
  // Amendment 13: the folder made by [Create a folder for me] says what it's for.
  const isDefaultFolder = useDefaultFolder()?.folder_id === live.id;

  useEffect(() => {
    if (confirm) keepRef.current?.focus();
  }, [confirm]);

  const act = (f: () => Promise<unknown>) => () => {
    f().catch((e) => showError(e, act(f)));
  };
  const sync = act(() => unwrap(commands.syncFolder(live.id)));
  const pause = act(() => unwrap(commands.setFolderPaused(live.id, true)));
  const resume = act(() => unwrap(commands.setFolderPaused(live.id, false)));
  const stop = act(() => unwrap(commands.stopFolder(live.id)));
  /** DS-5.3 [Include them]: the page 25 switch, from the note (saves at once; no re-read). */
  const includeOnlineOnly = async () => {
    setIncluding(true);
    try {
      const r = await unwrap(
        commands.updateFolderSettings(detail.id, { ...detail.settings, include_online_only: true }),
      );
      onSaved(r.detail);
      toast.success("Saved");
    } catch (e) {
      showError(e, () => void includeOnlineOnly());
    } finally {
      setIncluding(false);
    }
  };
  const onlineOnly = Math.max(detail.online_only_skipped, live.online_only_skipped);
  const cloudNote = detail.cloud_note ?? live.cloud_note;
  const remove = () => {
    setSending(true);
    unwrap(commands.removeFolder(live.id))
      .then(() => {
        setConfirm(false);
        void navigate({ to: "/folders" });
      })
      .catch((e) => showError(e, remove))
      .finally(() => setSending(false));
  };

  return (
    <Card className="space-y-3" aria-busy={removing || undefined}>
      <div className="flex items-start gap-3">
        <FolderStateIcon f={live} />
        <h1 className="min-w-0 break-words text-lg font-semibold leading-tight">{live.name}</h1>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <code className="break-all text-xs text-muted-foreground">{detail.path}</code>
        <Button size="sm" variant="ghost" onClick={() => void copyText(detail.path, "Path copied.")}>
          <ClipboardCopy /> Copy path
        </Button>
        {!removing && (
          // Amendment 13: opens the folder in File Explorer (nothing in it is run).
          <Button size="sm" variant="ghost" onClick={act(() => unwrap(commands.openFolder(live.id)))}>
            <FolderOpen /> Open folder
          </Button>
        )}
      </div>
      <p id="folder-line" className="text-sm text-muted-foreground">
        {live.line}
      </p>
      {isDefaultFolder && (
        <p className="flex items-start gap-1.5 text-sm">
          <Sparkles className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden /> Your folder is ready. Anything you
          put here becomes searchable automatically.
        </p>
      )}
      <FolderProgress folder={live} />
      {detail.watch_note && (
        <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
          <Network className="mt-px size-3.5 shrink-0" aria-hidden /> {detail.watch_note}
        </p>
      )}
      {live.was_files_only && live.state === "paused" && (
        <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
          <ListTree className="mt-px size-3.5 shrink-0" aria-hidden /> {WAS_FILES_ONLY_NOTE}
        </p>
      )}
      {cloudNote && (
        <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
          <Cloud className="mt-px size-3.5 shrink-0" aria-hidden /> {cloudNote}
        </p>
      )}
      {onlineOnly > 0 && !detail.settings.include_online_only && (
        <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
          <span className="flex items-start gap-1.5">
            <Cloud className="mt-px size-3.5 shrink-0" aria-hidden /> {ONLINE_ONLY_NOTE}
          </span>
          <Button size="sm" variant="outline" busy={including} onClick={() => void includeOnlineOnly()}>
            Include them
          </Button>
        </div>
      )}
      {detail.last_checked_ms !== null && (
        <p className="text-sm text-muted-foreground" title={absoluteTime(detail.last_checked_ms)}>
          Last checked {relativeTime(detail.last_checked_ms)}
        </p>
      )}
      {!removing && (
        <div className="flex flex-wrap items-center gap-2">
          {confirm ? (
            <div
              role="group"
              aria-label={`Remove “${live.name}”?`}
              className="flex flex-wrap items-center gap-2"
              onKeyDown={(e) => {
                if (e.key === "Escape") setConfirm(false);
              }}
            >
              <span className="text-sm">
                Stop searching this folder? It disappears from search right away. Your files stay where they are.
              </span>
              <Button size="sm" variant="danger" busy={sending} onClick={remove}>
                Remove
              </Button>
              <Button ref={keepRef} size="sm" variant="ghost" disabled={sending} onClick={() => setConfirm(false)}>
                Keep
              </Button>
            </div>
          ) : (
            <>
              {live.state === "stopped" ? (
                <Button size="sm" variant="outline" onClick={sync}>
                  Continue
                </Button>
              ) : (
                !busy &&
                live.state !== "paused" && (
                  <Button size="sm" variant="outline" onClick={sync}>
                    Check for changes now
                  </Button>
                )
              )}
              {live.state === "paused" ? (
                <Button size="sm" variant="outline" onClick={resume}>
                  {live.was_files_only ? SEARCH_ITS_CONTENTS : "Resume"}
                </Button>
              ) : (
                live.state !== "unavailable" &&
                live.state !== "stopped" && (
                  <Button size="sm" variant="ghost" onClick={pause}>
                    Pause
                  </Button>
                )
              )}
              {busy && (
                // OQ-39 (a): Stop lives only here, while busy.
                <Button size="sm" variant="ghost" onClick={stop}>
                  <Square /> Stop
                </Button>
              )}
              <Button size="sm" variant="ghost" onClick={() => setConfirm(true)}>
                Remove folder…
              </Button>
            </>
          )}
        </div>
      )}
    </Card>
  );
}

export function FolderDetailPage() {
  const { id } = useParams({ from: "/folders/$id" });
  const { section } = useSearch({ from: "/folders/$id" });
  const view = useAppView();
  const qc = useQueryClient();
  const live = view?.folders.find((f) => f.id === id);
  const key = [...FOLDER_DETAIL_KEY, id];
  const detail = useQuery({ queryKey: key, queryFn: () => unwrap(commands.getFolderDetail(id)) });

  // Push-driven re-read: this folder's state or skipped count changed (and it isn't mid-run).
  const settled = live !== undefined && !isBusy(live);
  const liveKey = live ? `${live.state}|${live.files_skipped}|${live.files_total}` : "gone";
  const first = useRef(true);
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    if (settled) void qc.invalidateQueries({ queryKey: [...FOLDER_DETAIL_KEY, id] });
  }, [liveKey, settled, qc, id]);

  const back = (
    <Link
      to="/folders"
      className="inline-flex items-center gap-1 rounded-sm text-sm text-primary hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
    >
      <ChevronLeft className="size-4" aria-hidden /> Folders
    </Link>
  );

  const notFound =
    (view && !live && view.overall !== "starting") ||
    (detail.error && asUiError(detail.error).details.startsWith("NotFound"));
  if (notFound) {
    return (
      <div className="space-y-4">
        {back}
        <Card>
          <EmptyState
            icon={FolderX}
            headingLevel={1}
            title="This folder isn't in Selene Retrieval anymore."
            action={
              <Button asChild>
                <Link to="/folders">Back to folders</Link>
              </Button>
            }
          />
        </Card>
      </div>
    );
  }

  return (
    <div className="space-y-4">
      {back}
      {detail.error && <ErrorPanel error={asUiError(detail.error)} onRetry={() => void detail.refetch()} />}
      {!detail.data || !live ? (
        <Card className="space-y-3" aria-busy="true">
          {/* The page title exists from the first frame, so focus can land on it (DS-6). */}
          {live ? (
            <h1 className="text-lg font-semibold leading-tight">{live.name}</h1>
          ) : (
            <div className="h-5 w-1/3 rounded-md bg-muted" />
          )}
          <div className="h-3 w-2/3 rounded-md bg-muted" />
          <div className="h-3 w-1/2 rounded-md bg-muted" />
        </Card>
      ) : (
        <>
          <HeaderCard live={live} detail={detail.data} onSaved={(d) => qc.setQueryData(key, d)} />
          {detail.data.skipped_total > 0 && <SkippedCard detail={detail.data} focus={section === "skipped"} />}
          <SettingsCards detail={detail.data} onSaved={(d) => qc.setQueryData(key, d)} />
        </>
      )}
    </div>
  );
}
