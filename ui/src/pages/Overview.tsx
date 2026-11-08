// Page 08 · Overview (M4 trim, OQ-36 a): a search box, at most 5 folder cards + "See all {n}
// folders", and the "Use it in Claude" card. Everything is pushed; nothing is read on a timer.
import { useState } from "react";
import { Link, Navigate, useNavigate } from "@tanstack/react-router";
import { useAddFolderFlow } from "@/components/AddFolderFlow";
import { ClaudeConnect } from "@/components/ClaudeConnect";
import { FolderCard } from "@/components/FolderCard";
import { Button } from "@/components/ui/button";
import { Card, CardTitle } from "@/components/ui/card";
import { useAppView } from "@/lib/app";
import { sortFolders } from "@/lib/folders";
import { startAddFolder } from "@/lib/store";
import { thousands } from "@/lib/format";
import { ChevronRight, FolderPlus, Info, Loader2, Search } from "lucide-react";

/** Folder cards shown on the Overview before "See all" (OQ-36 a). */
const OVERVIEW_MAX = 5;

function SearchBox() {
  const navigate = useNavigate();
  const [q, setQ] = useState("");
  return (
    <form
      role="search"
      className="flex gap-2"
      onSubmit={(e) => {
        e.preventDefault();
        if (q.trim()) void navigate({ to: "/search", search: { q: q.trim() } });
      }}
    >
      <label className="relative block flex-1">
        <Search className="pointer-events-none absolute left-3 top-1/2 size-4 -translate-y-1/2 text-muted-foreground" aria-hidden />
        <input
          value={q}
          onChange={(e) => setQ(e.target.value)}
          placeholder="Search your folders"
          aria-label="Search your folders"
          className="h-10 w-full rounded-md border border-control-off bg-background pl-9 pr-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
        />
      </label>
      <Button type="submit" className="h-10">
        Search
      </Button>
    </form>
  );
}

export function OverviewPage() {
  const view = useAppView();
  const flow = useAddFolderFlow();

  if (!view || view.overall === "starting") {
    return (
      <p className="flex items-center gap-2 py-10 text-muted-foreground" role="status">
        <Loader2 className="size-4 animate-spin" aria-hidden />{" "}
        {view?.restarting ? "Restarting the background service…" : "Starting Selene Retrieval…"}
      </p>
    );
  }
  if (view.overall === "no_folders") return <Navigate to="/welcome" />;

  const all = sortFolders(view.folders);
  const shown = all.slice(0, OVERVIEW_MAX);

  return (
    <div className="space-y-6">
      <SearchBox />
      <section className="space-y-3" aria-labelledby="folders-h">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <h1 id="folders-h" className="text-lg font-semibold">
            Your folders
          </h1>
          <Button id="add-folder" variant="outline" size="sm" onClick={() => startAddFolder()}>
            <FolderPlus /> Add a folder
          </Button>
        </div>
        {view.catching_up && (
          <p className="flex items-center gap-2 text-sm text-muted-foreground">
            <Info className="size-4 shrink-0" aria-hidden />
            Search works meanwhile; recent changes appear as they are applied.
          </p>
        )}
        {flow.panel}
        <ul className="grid gap-3" aria-label="Folders">
          {shown.map((f) => (
            <li key={f.id} data-folder-id={f.id}>
              <FolderCard folder={f} />
            </li>
          ))}
        </ul>
        {all.length > OVERVIEW_MAX && (
          <Link
            to="/folders"
            className="inline-flex items-center gap-1 rounded-sm text-sm text-primary underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
          >
            See all {thousands(all.length)} folders <ChevronRight className="size-4" aria-hidden />
          </Link>
        )}
      </section>
      <Card className="space-y-3">
        <CardTitle>Use it in Claude</CardTitle>
        <ClaudeConnect />
      </Card>
    </div>
  );
}
