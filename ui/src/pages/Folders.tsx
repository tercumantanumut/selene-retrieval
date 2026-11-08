// Page 24 · Folders: every folder in a compact list (56 px rows) that scales to dozens; each row
// opens the folder's detail. Rows update by push (`AppView.folders`); nothing is read on a timer.
// Amendment 13: [Create a folder for me] in the empty state, and next to [Add a folder] while the
// default folder isn't added; success opens the new folder's details.
import { useId, useRef, useState, type KeyboardEvent } from "react";
import { useQuery } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { toast } from "sonner";
import { commands, type FolderView, type GroupRow } from "@/bindings";
import { useAddFolderFlow } from "@/components/AddFolderFlow";
import { EmptyState } from "@/components/EmptyState";
import { ErrorPanel } from "@/components/ErrorPanel";
import { FolderRootsHint } from "@/components/Managed";
import { FolderStateIcon } from "@/components/FolderCard";
import { Button } from "@/components/ui/button";
import { Card } from "@/components/ui/card";
import { GROUPS_KEY, unwrap, useAppView } from "@/lib/app";
import { defaultFolderHint, useCreateDefaultFolder, useDefaultFolder } from "@/lib/defaultFolder";
import { sortFolders } from "@/lib/folders";
import { thousands } from "@/lib/format";
import { startAddFolder } from "@/lib/store";
import { ChevronRight, Cloud, FolderPlus, Network, Search } from "lucide-react";

/** The filter box appears once the list is long (page 24). */
const FILTER_FROM = 9;

function Badge({ icon: Icon, text }: { icon: typeof Network; text: string }) {
  return (
    <span data-surface className="inline-flex shrink-0 items-center gap-1.5 rounded-full bg-muted px-2 py-0.5 text-xs">
      <Icon className="size-3.5" aria-hidden />
      {text}
    </span>
  );
}

function rowName(f: FolderView): string {
  const notes = [
    f.note ? "Network drive" : null,
    f.online_only_skipped > 0 ? "OneDrive files skipped" : null,
  ].filter(Boolean);
  return [f.name, f.line, ...notes].join(", ");
}

/** DS-4.3 `row` variant: one link per folder. */
function FolderRow({ folder: f }: { folder: FolderView }) {
  return (
    <Link
      to="/folders/$id"
      params={{ id: f.id }}
      search={{}}
      data-row
      aria-label={rowName(f)}
      className="flex min-h-14 items-center gap-3 px-4 py-2 hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-primary"
    >
      <FolderStateIcon f={f} />
      <span className="min-w-0 flex-1">
        <span className="flex min-w-0 items-baseline gap-2">
          {/* Name and line share the row; at 200 % text both truncate instead of overflowing. */}
          <span className="min-w-0 max-w-[60%] shrink-0 truncate font-semibold" title={f.name}>
            {f.name}
          </span>
          <span className="truncate text-sm text-muted-foreground">{f.line}</span>
        </span>
        {f.path && (
          // The path line shows only at ≥ 760 px (page 24).
          <span className="hidden truncate text-xs text-muted-foreground min-[760px]:block" title={f.path}>
            {f.path}
          </span>
        )}
      </span>
      {f.note && <Badge icon={Network} text="Network drive" />}
      {f.online_only_skipped > 0 && <Badge icon={Cloud} text="OneDrive files skipped" />}
      <ChevronRight className="size-4 shrink-0 text-muted-foreground" aria-hidden />
    </Link>
  );
}

/** ↑/↓ move between rows, Home/End go to the ends (page 24). */
function onListKey(e: KeyboardEvent<HTMLElement>) {
  if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(e.key)) return;
  const rows = Array.from(e.currentTarget.querySelectorAll<HTMLElement>("[data-row]"));
  const i = rows.indexOf(document.activeElement as HTMLElement);
  if (i < 0) return;
  e.preventDefault();
  const next =
    e.key === "Home" ? 0 : e.key === "End" ? rows.length - 1 : e.key === "ArrowDown" ? Math.min(i + 1, rows.length - 1) : Math.max(i - 1, 0);
  rows[next]?.focus();
}

function Rows({ folders }: { folders: FolderView[] }) {
  return (
    <ul className="divide-y divide-border" aria-label="Folders">
      {folders.map((f) => (
        <li key={f.id} data-folder-id={f.id}>
          <FolderRow folder={f} />
        </li>
      ))}
    </ul>
  );
}

export function FoldersPage() {
  const view = useAppView();
  const flow = useAddFolderFlow();
  const navigate = useNavigate();
  const offer = useDefaultFolder();
  const maker = useCreateDefaultFolder();
  const offered = offer?.available === true;
  const createFolder = async () => {
    const res = await maker.create();
    if (!res) return;
    toast.success(res.already_added ? "Your folder is already searchable" : "Your folder is ready", {
      description: `Anything you put in “${res.name}” becomes searchable automatically.`,
    });
    void navigate({ to: "/folders/$id", params: { id: res.folder_id }, search: {} });
  };
  // Its failure panel: the one action is always choosing a folder yourself.
  const makerPanel = maker.error && (
    <ErrorPanel
      error={maker.error}
      onAction={
        maker.error.action.kind === "choose_folder"
          ? () => {
              maker.clearError();
              startAddFolder();
            }
          : undefined
      }
      onDismiss={() => maker.clearError()}
    >
      <FolderRootsHint details={maker.error.details} />
    </ErrorPanel>
  );
  const [filter, setFilter] = useState("");
  const filterRef = useRef<HTMLInputElement>(null);
  const ids = useId();
  // Folder groups (Advanced): rows are grouped only when more than one group exists.
  const groups = useQuery({ queryKey: GROUPS_KEY, queryFn: () => unwrap(commands.getGroups()) }).data;

  const header = (showFilter: boolean) => (
    <div className="flex flex-wrap items-center justify-between gap-3">
      <h1 className="text-lg font-semibold">Folders</h1>
      <div className="flex flex-wrap items-center gap-2">
        {showFilter && (
          <label className="relative block">
            <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" aria-hidden />
            <input
              ref={filterRef}
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder="Filter folders"
              aria-label="Filter folders"
              aria-describedby={`${ids}-count`}
              className="h-9 w-56 rounded-md border border-control-off bg-background pl-9 pr-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
            />
          </label>
        )}
        {offered && (
          <Button variant="ghost" size="sm" busy={maker.busy} onClick={() => void createFolder()}>
            Create a folder for me
          </Button>
        )}
        <Button id="add-folder" variant="outline" size="sm" onClick={() => startAddFolder()}>
          <FolderPlus /> Add a folder
        </Button>
      </div>
    </div>
  );

  if (!view) {
    // Loading: three static skeleton rows (no shimmer).
    return (
      <div className="space-y-4">
        {header(false)}
        <Card className="space-y-4 p-4" aria-busy="true">
          {[0, 1, 2].map((i) => (
            <div key={i} className="h-3 w-2/3 rounded-md bg-muted" />
          ))}
        </Card>
      </div>
    );
  }

  const all = sortFolders(view.folders);
  if (all.length === 0) {
    return (
      <div className="space-y-4">
        <h1 className="text-lg font-semibold">Folders</h1>
        {flow.panel}
        {makerPanel}
        <Card>
          {offered && offer ? (
            <EmptyState
              icon={FolderPlus}
              title="No folders yet"
              text="Create a folder in Documents for your files, or choose one you already have."
              action={
                <div className="flex flex-wrap justify-center gap-3">
                  <Button
                    busy={maker.busy}
                    aria-describedby="create-folder-hint"
                    onClick={() => void createFolder()}
                  >
                    Create a folder for me
                  </Button>
                  <Button id="add-folder" variant="outline" disabled={maker.busy} onClick={() => startAddFolder()}>
                    Choose an existing folder…
                  </Button>
                  <p id="create-folder-hint" className="w-full text-sm text-muted-foreground">
                    {defaultFolderHint(offer.name)}
                  </p>
                </div>
              }
            />
          ) : (
            <EmptyState
              icon={FolderPlus}
              title="No folders yet"
              text="Add a folder to make it searchable."
              action={
                <Button id="add-folder" onClick={() => startAddFolder()}>
                  Add a folder
                </Button>
              }
            />
          )}
        </Card>
      </div>
    );
  }

  const q = filter.trim().toLowerCase();
  const shown = q
    ? all.filter((f) => f.name.toLowerCase().includes(q) || (f.path ?? "").toLowerCase().includes(q))
    : all;
  const total = all.reduce((n, f) => n + (f.state === "removing" ? 0 : f.files_total), 0);
  const grouped = (groups?.length ?? 0) > 1;
  const known = new Set((groups ?? []).map((g) => g.id));
  // A folder whose group isn't listed (older service, a group just deleted) shows under the default.
  const byGroup = (g: GroupRow) =>
    shown.filter((f) => f.group_id === g.id || (g.is_default && (!f.group_id || !known.has(f.group_id))));

  return (
    <div className="space-y-4">
      {header(all.length >= FILTER_FROM || filter !== "")}
      <p id={`${ids}-count`} className="sr-only" aria-live="polite">
        {q ? `${thousands(shown.length)} folder${shown.length === 1 ? "" : "s"} shown` : ""}
      </p>
      {flow.panel}
      {makerPanel}
      {shown.length === 0 ? (
        <Card>
          <div role="status" className="flex flex-col items-center gap-3 py-8 text-center">
            <Search className="size-8 text-muted-foreground" strokeWidth={1.5} aria-hidden />
            <p className="text-sm">No folder matches “{filter.trim()}”.</p>
            <Button
              variant="outline"
              size="sm"
              onClick={() => {
                setFilter("");
                filterRef.current?.focus();
              }}
            >
              Clear filter
            </Button>
          </div>
        </Card>
      ) : (
        <Card className="overflow-hidden p-0" onKeyDown={onListKey}>
          {grouped && groups ? (
            groups.map((g) => {
              const rows = byGroup(g);
              if (rows.length === 0) return null;
              return (
                <section key={g.id} aria-label={g.name} className="border-b border-border last:border-b-0">
                  <h2 className="bg-muted px-4 py-1.5 text-xs font-semibold text-muted-foreground">{g.name}</h2>
                  <Rows folders={rows} />
                </section>
              );
            })
          ) : (
            <Rows folders={shown} />
          )}
        </Card>
      )}
      <p className="text-xs text-muted-foreground">
        {thousands(all.length)} folder{all.length === 1 ? "" : "s"} · {thousands(total)} files
      </p>
    </div>
  );
}
