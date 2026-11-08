// Page 30 · Activity and data sent to AI providers. Read-only and never needed for a task. Read
// on open, on "Show more", and once after a pushed `activity_added` / `egress_recorded` signal.
import { useId, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { commands, type ActivityIcon, type ActivityRow, type EgressRowView } from "@/bindings";
import { EmptyState } from "@/components/EmptyState";
import { ErrorPanel } from "@/components/ErrorPanel";
import { Button } from "@/components/ui/button";
import { Card, CardTitle } from "@/components/ui/card";
import { Select } from "@/components/ui/select";
import { ACTIVITY_KEY, asUiError, EGRESS_KEY, unwrap, useAppView, useErrorToast } from "@/lib/app";
import { absoluteTime, clock, dayAndTime, dayHeading } from "@/lib/time";
import {
  AlertTriangle,
  CheckCircle2,
  Clock,
  Download,
  FolderInput,
  FolderMinus,
  FolderPlus,
  Plug,
  RefreshCw,
  Sparkles,
} from "lucide-react";

const ICONS: Record<ActivityIcon, typeof Clock> = {
  folder_added: FolderPlus,
  folder_removed: FolderMinus,
  up_to_date: CheckCircle2,
  skipped: AlertTriangle,
  upgraded: Sparkles,
  app: Plug,
  request: FolderInput,
  updated: Download,
  moved: RefreshCw,
};

const ALL = "__all__";

function Row({ row, folderExists }: { row: ActivityRow; folderExists: boolean }) {
  const Icon = ICONS[row.icon];
  const text =
    row.folder_id && folderExists ? (
      <Link
        to="/folders/$id"
        params={{ id: row.folder_id }}
        search={row.to_skipped ? { section: "skipped" } : {}}
        className="rounded-sm text-primary hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary"
      >
        {row.text}
      </Link>
    ) : (
      row.text
    );
  return (
    <li className="flex items-start justify-between gap-3 py-1.5 text-sm">
      <span className="flex min-w-0 items-start gap-2">
        <Icon className="mt-0.5 size-4 shrink-0 text-muted-foreground" aria-hidden />
        <span className="min-w-0 break-words">{text}</span>
      </span>
      {row.ts_ms !== null && (
        <span className="shrink-0 text-xs text-muted-foreground" title={absoluteTime(row.ts_ms)}>
          {clock(row.ts_ms)}
        </span>
      )}
    </li>
  );
}

function RecentActivity() {
  const view = useAppView();
  const showError = useErrorToast();
  const first = useQuery({ queryKey: [...ACTIVITY_KEY, "first"], queryFn: () => unwrap(commands.getActivity(null)) });
  const [older, setOlder] = useState<ActivityRow[]>([]);
  const [next, setNext] = useState<number | null | undefined>(undefined);
  const [loading, setLoading] = useState(false);
  const known = new Set((view?.folders ?? []).map((f) => f.id));

  if (first.error) return <ErrorPanel error={asUiError(first.error)} onRetry={() => void first.refetch()} />;
  if (!first.data) {
    return (
      <Card className="space-y-3" aria-busy="true">
        {[0, 1, 2].map((i) => (
          <div key={i} className="h-3 w-2/3 rounded-md bg-muted" />
        ))}
      </Card>
    );
  }
  const seen = new Set(first.data.rows.map((r) => r.id));
  const rows = [...first.data.rows, ...older.filter((r) => !seen.has(r.id))];
  const cursor = next === undefined ? first.data.next_before_id : next;
  if (rows.length === 0) {
    return (
      <Card>
        <EmptyState icon={Clock} title="Nothing yet" text="Activity from the last 7 days appears here." />
      </Card>
    );
  }
  const more = async () => {
    if (cursor === null) return;
    setLoading(true);
    try {
      const p = await unwrap(commands.getActivity(cursor));
      setOlder((o) => [...o, ...p.rows]);
      setNext(p.next_before_id);
    } catch (e) {
      showError(e, () => void more());
    } finally {
      setLoading(false);
    }
  };
  // Group by day heading, newest first (the service's order).
  const days: { heading: string; rows: ActivityRow[] }[] = [];
  for (const r of rows) {
    const h = r.ts_ms !== null ? dayHeading(r.ts_ms) : "";
    const last = days[days.length - 1];
    if (last && last.heading === h) last.rows.push(r);
    else days.push({ heading: h, rows: [r] });
  }
  return (
    <Card className="space-y-3">
      <CardTitle>Recent activity</CardTitle>
      {days.map((d) => (
        <section key={d.heading} className="space-y-1">
          <h3 className="text-xs font-semibold text-muted-foreground">{d.heading}</h3>
          <ul className="divide-y divide-border">
            {d.rows.map((r) => (
              <Row key={r.id ?? r.text} row={r} folderExists={r.folder_id !== null && known.has(r.folder_id)} />
            ))}
          </ul>
        </section>
      ))}
      {cursor !== null && cursor !== undefined && (
        <Button size="sm" variant="outline" busy={loading} onClick={() => void more()}>
          Show more
        </Button>
      )}
    </Card>
  );
}

function SentToProviders() {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const ids = useId();
  const [provider, setProvider] = useState<string | null>(null);
  const key = [...EGRESS_KEY, provider ?? ALL];
  const first = useQuery({ queryKey: key, queryFn: () => unwrap(commands.getEgress(null, provider)) });
  const [older, setOlder] = useState<EgressRowView[]>([]);
  const [next, setNext] = useState<number | null | undefined>(undefined);
  const [loading, setLoading] = useState(false);
  if (first.error) return <ErrorPanel error={asUiError(first.error)} onRetry={() => void first.refetch()} />;
  // Hidden when no provider was ever used.
  if (!first.data || first.data.providers.length === 0) return null;
  const seen = new Set(first.data.rows.map((r) => r.id));
  const rows = [...first.data.rows, ...older.filter((r) => !seen.has(r.id))];
  const cursor = next === undefined ? first.data.next_before_id : next;
  const choose = (v: string) => {
    setProvider(v === ALL ? null : v);
    setOlder([]);
    setNext(undefined);
    void qc.invalidateQueries({ queryKey: EGRESS_KEY });
  };
  const more = async () => {
    if (cursor === null || cursor === undefined) return;
    setLoading(true);
    try {
      const p = await unwrap(commands.getEgress(cursor, provider));
      setOlder((o) => [...o, ...p.rows]);
      setNext(p.next_before_id);
    } catch (e) {
      showError(e, () => void more());
    } finally {
      setLoading(false);
    }
  };
  return (
    <Card className="space-y-3">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <CardTitle id={`${ids}-h`}>Sent to AI providers</CardTitle>
        {first.data.providers.length > 1 && (
          <div className="w-48">
            <span id={`${ids}-f`} className="sr-only">
              Filter by provider
            </span>
            <Select
              labelledBy={`${ids}-f`}
              value={provider ?? ALL}
              options={[{ value: ALL, label: "All providers" }, ...first.data.providers.map((p) => ({ value: p.id, label: p.name }))]}
              placeholder="All providers"
              onChange={choose}
            />
          </div>
        )}
      </div>
      {rows.length === 0 ? (
        <p className="text-sm text-muted-foreground">Nothing has been sent.</p>
      ) : (
        <div className="overflow-x-auto rounded-md border border-border">
          <table className="w-full border-collapse text-sm">
            <caption className="sr-only">Sent to AI providers</caption>
            <thead>
              <tr className="border-b border-border text-left text-xs text-muted-foreground">
                <th scope="col" className="px-3 py-2 font-medium">Time</th>
                <th scope="col" className="px-3 py-2 font-medium">Provider</th>
                <th scope="col" className="px-3 py-2 font-medium">What</th>
                <th scope="col" className="px-3 py-2 font-medium">Size</th>
                <th scope="col" className="px-3 py-2 font-medium">Requested by</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.id ?? `${r.ts_ms}-${r.what}`} className="border-b border-border last:border-b-0">
                  <td className="whitespace-nowrap px-3 py-2" title={r.ts_ms !== null ? absoluteTime(r.ts_ms) : undefined}>
                    {r.ts_ms !== null ? dayAndTime(r.ts_ms) : ""}
                  </td>
                  <td className="px-3 py-2">{r.provider_name}</td>
                  <td className="px-3 py-2">{r.what}</td>
                  <td className="whitespace-nowrap px-3 py-2">{r.size}</td>
                  <td className="px-3 py-2">{r.requested_by}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      {cursor !== null && cursor !== undefined && (
        <Button size="sm" variant="outline" busy={loading} onClick={() => void more()}>
          Show more
        </Button>
      )}
      <p className="text-xs text-muted-foreground">Search on this computer is never listed here, because it sends nothing.</p>
    </Card>
  );
}

export function ActivityPage() {
  return (
    <div className="space-y-4">
      <h1 className="text-lg font-semibold">Activity</h1>
      <RecentActivity />
      <SentToProviders />
    </div>
  );
}
