// Page 31 · Folder groups (Advanced only; Settings → Advanced → Folder groups). Groups are labels
// over folders (contract §3): they decide which apps may see which folders. All groups use the
// one search model, so [Change search model] opens Settings → Search quality. Never needed for
// basic use; the Basic UI never says "group".
import { useEffect, useId, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { toast } from "sonner";
import { commands, type GroupRow, type TierName } from "@/bindings";
import { ErrorPanel } from "@/components/ErrorPanel";
import { Button } from "@/components/ui/button";
import { Card, CardText } from "@/components/ui/card";
import { asUiError, GROUPS_KEY, SERVICE_CLIENTS_KEY, unwrap, useAppView, useErrorToast } from "@/lib/app";
import { thousands } from "@/lib/format";
import { AlertTriangle, ChevronLeft } from "lucide-react";

const TIER_WORD: Record<TierName, string> = { basic: "Basic search", better: "Better search", best: "Best search" };

function NameForm({
  initial,
  submitLabel,
  onSubmit,
  onCancel,
  label,
  saveOnBlur = false,
}: {
  initial: string;
  submitLabel: string;
  onSubmit: (name: string) => Promise<string | null>;
  onCancel?: () => void;
  label: string;
  /** Rename saves on blur too (page 31); Create needs the button or Enter. */
  saveOnBlur?: boolean;
}) {
  const ids = useId();
  const [name, setName] = useState(initial);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const ref = useRef<HTMLInputElement>(null);
  useEffect(() => {
    ref.current?.focus();
    ref.current?.select();
  }, []);
  const submit = async () => {
    if (!name.trim()) return;
    setBusy(true);
    setError(await onSubmit(name));
    setBusy(false);
  };
  return (
    <form
      className="space-y-1.5"
      onSubmit={(e) => {
        e.preventDefault();
        void submit();
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape") onCancel?.();
      }}
    >
      <label htmlFor={`${ids}-n`} className="block text-sm font-medium">
        {label}
      </label>
      <div className="flex flex-wrap gap-2">
        <input
          ref={ref}
          id={`${ids}-n`}
          value={name}
          maxLength={80}
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? `${ids}-e` : undefined}
          onChange={(e) => {
            setName(e.target.value);
            setError(null);
          }}
          onBlur={(e) => {
            // Moving to [Save] or [Cancel] isn't "done typing": Cancel must not save (P2 finding).
            if (e.relatedTarget instanceof Node && e.currentTarget.form?.contains(e.relatedTarget)) return;
            if (saveOnBlur && name.trim() && name !== initial) void submit();
          }}
          className="h-9 min-w-0 flex-1 rounded-md border border-control-off bg-background px-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
        />
        <Button type="submit" size="sm" className="h-9" busy={busy} disabled={!name.trim()}>
          {submitLabel}
        </Button>
        {onCancel && (
          <Button type="button" size="sm" variant="ghost" className="h-9" onClick={onCancel}>
            Cancel
          </Button>
        )}
      </div>
      {error && (
        <p id={`${ids}-e`} className="flex items-center gap-1.5 text-xs text-warning-strong">
          <AlertTriangle className="size-3.5 shrink-0" aria-hidden /> {error}
        </p>
      )}
    </form>
  );
}

function GroupCard({ group, model, defaultName }: { group: GroupRow; model: string; defaultName: string }) {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const showError = useErrorToast();
  const ids = useId();
  const [renaming, setRenaming] = useState(false);
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const keepRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (confirm) keepRef.current?.focus();
  }, [confirm]);
  const n = group.folder_ids.length;

  const rename = async (name: string): Promise<string | null> => {
    try {
      await unwrap(commands.renameGroup(group.id, name));
      setRenaming(false);
      void qc.invalidateQueries({ queryKey: GROUPS_KEY });
      return null;
    } catch (e) {
      return asUiError(e).body;
    }
  };
  const remove = async () => {
    setBusy(true);
    try {
      const r = await unwrap(commands.deleteGroup(group.id));
      void qc.invalidateQueries({ queryKey: GROUPS_KEY });
      void qc.invalidateQueries({ queryKey: SERVICE_CLIENTS_KEY });
      // Fail closed (P1.7): an app limited to this group now sees nothing. Say so plainly.
      for (const app of r.apps_without_access) {
        toast.info(`${app} no longer has access to any folders. Choose its folders in Connected apps.`, {
          action: { label: "Open Connected apps", onClick: () => void navigate({ to: "/connected-apps" }) },
          duration: Infinity,
        });
      }
    } catch (e) {
      showError(e, () => void remove());
    } finally {
      setBusy(false);
      setConfirm(false);
    }
  };

  return (
    <Card className="space-y-2" role="region" aria-labelledby={`${ids}-h`}>
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <h2 id={`${ids}-h`} className="text-base font-semibold">
          {group.name}
        </h2>
        {group.is_default && <span className="text-xs text-muted-foreground">Default · new folders go here</span>}
      </div>
      <CardText>
        {thousands(n)} folder{n === 1 ? "" : "s"} · Search model: {model}
      </CardText>
      {renaming ? (
        <NameForm
          initial={group.name}
          submitLabel="Save"
          label="Group name"
          saveOnBlur
          onSubmit={rename}
          onCancel={() => setRenaming(false)}
        />
      ) : confirm ? (
        <div
          role="group"
          aria-label={`Delete “${group.name}”?`}
          className="flex flex-wrap items-center gap-2"
          onKeyDown={(e) => {
            if (e.key === "Escape") setConfirm(false);
          }}
        >
          <span className="text-sm">
            Delete “{group.name}”? Its folders move to “{defaultName}”. Nothing is removed from search.
          </span>
          <Button size="sm" variant="danger" busy={busy} onClick={() => void remove()}>
            Delete
          </Button>
          <Button ref={keepRef} size="sm" variant="ghost" disabled={busy} onClick={() => setConfirm(false)}>
            Keep
          </Button>
        </div>
      ) : (
        <div className="flex flex-wrap gap-2">
          <Button size="sm" variant="outline" onClick={() => setRenaming(true)}>
            Rename
          </Button>
          <Button size="sm" variant="ghost" onClick={() => void navigate({ to: "/settings", search: { section: "search" } })}>
            Change search model
          </Button>
          {!group.is_default && (
            <Button size="sm" variant="ghost" onClick={() => setConfirm(true)}>
              Delete…
            </Button>
          )}
        </div>
      )}
    </Card>
  );
}

export function FolderGroupsPage() {
  const view = useAppView();
  const qc = useQueryClient();
  const groups = useQuery({ queryKey: GROUPS_KEY, queryFn: () => unwrap(commands.getGroups()) });
  const [creating, setCreating] = useState(false);
  const model = view?.tier ? (view.tier.multilingual ? "Multilingual search" : TIER_WORD[view.tier.active]) : "Basic search";

  const create = async (name: string): Promise<string | null> => {
    try {
      await unwrap(commands.createGroup(name));
      setCreating(false);
      void qc.invalidateQueries({ queryKey: GROUPS_KEY });
      return null;
    } catch (e) {
      return asUiError(e).body;
    }
  };

  return (
    <div className="space-y-4">
      <Link
        to="/settings"
        className="inline-flex items-center gap-1 rounded-sm text-sm text-primary hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary"
      >
        <ChevronLeft className="size-4" aria-hidden /> Settings
      </Link>
      <h1 className="text-lg font-semibold">Folder groups</h1>
      <p className="text-sm text-muted-foreground">
        {/* Brief copy trimmed: in M4 every group uses the one search model (contract §3). */}
        Groups let you give apps access to only some folders. Most people don't need them.
      </p>
      {groups.error && <ErrorPanel error={asUiError(groups.error)} onRetry={() => void groups.refetch()} />}
      {groups.data?.map((g) => (
        <GroupCard
          key={g.id}
          group={g}
          model={model}
          defaultName={groups.data?.find((x) => x.is_default)?.name ?? "My folders"}
        />
      ))}
      {creating ? (
        <Card>
          <NameForm initial="" submitLabel="Create" label="Group name" onSubmit={create} onCancel={() => setCreating(false)} />
        </Card>
      ) : (
        groups.data && (
          <Button variant="outline" onClick={() => setCreating(true)}>
            New group
          </Button>
        )
      )}
    </div>
  );
}
