// Page 29 · Connected apps: connect every AI app in one click (or with setup text), see which ones
// use Selene Retrieval, and (Advanced) choose what each may see and allow local tools over HTTP.
// Card states are pushed (`ClientsChanged`, `ServicePush clients_changed`); nothing is polled.
import { useEffect, useId, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "@tanstack/react-router";
import { toast } from "sonner";
import {
  commands,
  type ClientStatus,
  type GroupRow,
  type ScopeView,
  type ServiceClient,
  type TokenView,
} from "@/bindings";
import { ApprovalCard } from "@/components/ApprovalCard";
import { ClaudeConnect, RepairConfirm } from "@/components/ClaudeConnect";
import { ErrorPanel } from "@/components/ErrorPanel";
import { SnippetBlock } from "@/components/SnippetBlock";
import { Button } from "@/components/ui/button";
import { Card, CardText, CardTitle } from "@/components/ui/card";
import { Modal } from "@/components/ui/dialog";
import { Disclosure } from "@/components/ui/disclosure";
import { Select } from "@/components/ui/select";
import { SwitchRow } from "@/components/ui/switch-row";
import {
  ACTIVITY_KEY,
  ADVANCED_KEY,
  asUiError,
  CLAUDE_KEY,
  CLIENTS_KEY,
  copySnippet,
  GROUPS_KEY,
  SERVICE_CLIENTS_KEY,
  unwrap,
  useAppView,
  useErrorToast,
} from "@/lib/app";
import { PATHS, usePolicy } from "@/lib/policy";
import { absoluteTime, dayAndTime, relativeTime } from "@/lib/time";
import { CheckCircle2, KeyRound, Plug } from "lucide-react";

function LastUsed({ ms }: { ms: number | null }) {
  if (ms === null) return null;
  return (
    <span className="text-xs text-muted-foreground" title={absoluteTime(ms)}>
      Last used {relativeTime(ms)}
    </span>
  );
}

/** Cursor and VS Code (DS-5.5 states, in each app's words). */
function AppCard({ app }: { app: ClientStatus }) {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const ids = useId();
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [repairing, setRepairing] = useState(false);
  const name = app.name;
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: CLIENTS_KEY });
    void qc.invalidateQueries({ queryKey: CLAUDE_KEY });
  };
  const connect = async () => {
    setBusy(true);
    try {
      setMessage(await unwrap(commands.connectClient(app.app)));
      refresh();
    } catch (e) {
      showError(e, () => void connect());
    } finally {
      setBusy(false);
    }
  };
  const disconnect = async () => {
    setBusy(true);
    try {
      toast.success(await unwrap(commands.disconnectClient(app.app)));
      setMessage(null);
      refresh();
    } catch (e) {
      showError(e, () => void disconnect());
    } finally {
      setBusy(false);
    }
  };
  const copy = () => void copySnippet(app.snippet);

  return (
    <Card className="space-y-3" role="region" aria-labelledby={`${ids}-h`}>
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <CardTitle id={`${ids}-h`}>{name}</CardTitle>
        {app.state === "connected" ? (
          <LastUsed ms={app.last_used_ms} />
        ) : app.state === "not_connected" ? (
          <span className="text-xs text-muted-foreground">Not connected</span>
        ) : null}
      </div>
      {app.state === "not_installed" && <CardText>{name} isn't installed on this computer.</CardText>}
      {app.state === "connected" && (
        <div className="space-y-2">
          <p className="flex items-center gap-2 font-medium">
            <CheckCircle2 className="size-5 text-success" aria-hidden /> Connected
          </p>
          {message && <CardText>{message}</CardText>}
          <Button size="sm" variant="ghost" disabled={busy} onClick={() => void disconnect()}>
            Disconnect
          </Button>
        </div>
      )}
      {(app.state === "not_connected" || app.state === "needs_reconnect") && (
        <div className="space-y-3">
          {app.state === "needs_reconnect" && (
            <CardText>{name}'s connection points to an old copy of Selene Retrieval. Reconnect to fix it.</CardText>
          )}
          <div className="flex flex-wrap items-center gap-3">
            <Button disabled={busy} onClick={() => void connect()}>
              <Plug /> {app.state === "needs_reconnect" ? `Reconnect ${name}` : `Connect ${name}`}
            </Button>
            <Button variant="link" onClick={copy}>
              Copy setup text instead
            </Button>
          </div>
        </div>
      )}
      {app.state === "cant_read" && (
        <div className="space-y-3">
          <CardText>
            {name}'s settings file couldn't be read, so we didn't change it. Copy the setup text and paste it into{" "}
            {name}'s settings instead.
          </CardText>
          {repairing ? (
            <RepairConfirm app={app.app} short={name} onDone={() => setRepairing(false)} />
          ) : (
            <div className="flex flex-wrap gap-2">
              <Button variant="outline" onClick={copy}>
                Copy setup text
              </Button>
              <Button variant="ghost" onClick={() => setRepairing(true)}>
                Repair…
              </Button>
            </div>
          )}
        </div>
      )}
    </Card>
  );
}

/** Pending folder requests + the last 10 decisions (only while the switch is on). */
function FolderRequests() {
  const view = useAppView();
  const qc = useQueryClient();
  const showError = useErrorToast();
  const policy = usePolicy();
  const adv = useQuery({ queryKey: ADVANCED_KEY, queryFn: () => unwrap(commands.getAdvancedSettings()) }).data;
  const activity = useQuery({
    queryKey: [...ACTIVITY_KEY, "first"],
    queryFn: () => unwrap(commands.getActivity(null)),
    enabled: adv?.allow_app_folder_requests === true,
  }).data;
  if (!adv?.allow_app_folder_requests) return null;
  const decisions = (activity?.rows ?? []).filter((r) => r.icon === "request").slice(0, 10);
  const set = async (on: boolean) => {
    try {
      qc.setQueryData(ADVANCED_KEY, await unwrap(commands.setAdvancedSettings(null, on, null)));
    } catch (e) {
      showError(e, () => void set(on));
    }
  };
  return (
    <Card className="space-y-3">
      <CardTitle>Folder requests</CardTitle>
      <SwitchRow
        label="Let connected apps ask to add folders"
        help="Off by default. When on, Claude can ask (restart Claude to see it); nothing is added until you allow it here."
        checked
        managed={policy.field(PATHS.folderRequests)}
        onChange={(v) => void set(v)}
      />
      {(view?.approvals ?? []).map((a) => (
        <ApprovalCard key={a.id} approval={a} copy />
      ))}
      {decisions.length > 0 && (
        <ul className="space-y-1 text-sm text-muted-foreground">
          {decisions.map((d) => (
            <li key={d.id ?? d.text}>
              {d.text}
              {d.ts_ms !== null && <span title={absoluteTime(d.ts_ms)}> · {dayAndTime(d.ts_ms)}</span>}
            </li>
          ))}
        </ul>
      )}
    </Card>
  );
}

/**
 * M5 (contract §4 `clients.forget`, M4 tester P2): [Remove…] for an app that isn't connected
 * right now, so stale rows can be cleared. Inline confirm; focus goes to [Keep] (DS-4.9).
 */
function ForgetApp({ client, labelId }: { client: ServiceClient; labelId: string }) {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const keepRef = useRef<HTMLButtonElement>(null);
  const removeRef = useRef<HTMLButtonElement>(null);
  const reopened = useRef(false);
  useEffect(() => {
    if (confirm) keepRef.current?.focus();
    else if (reopened.current) removeRef.current?.focus();
  }, [confirm]);
  const keep = () => {
    reopened.current = true;
    setConfirm(false);
  };
  const forget = async () => {
    setBusy(true);
    try {
      await unwrap(commands.forgetClient(client.client_id));
      void qc.invalidateQueries({ queryKey: SERVICE_CLIENTS_KEY });
      void qc.invalidateQueries({ queryKey: CLIENTS_KEY });
      toast.success(`${client.name} was removed from this list.`);
      // The row leaves the list: focus the section heading so it isn't lost.
      document.getElementById("folder-access-h")?.focus();
    } catch (e) {
      showError(e, () => void forget());
    } finally {
      setBusy(false);
    }
  };
  if (!confirm) {
    return (
      // The visible label is the name (WCAG 2.5.3); which app it is comes from the row's name.
      <Button ref={removeRef} size="sm" variant="ghost" aria-describedby={labelId} onClick={() => setConfirm(true)}>
        Remove…
      </Button>
    );
  }
  return (
    <div
      role="group"
      aria-label={`Remove ${client.name}?`}
      className="flex flex-wrap items-center gap-2"
      onKeyDown={(e) => {
        if (e.key === "Escape") keep();
      }}
    >
      <span className="text-sm">
        Remove {client.name} from this list? If it uses Selene Retrieval again, it comes back with access to all your
        folders.
      </span>
      <Button size="sm" variant="danger" busy={busy} onClick={() => void forget()}>
        Remove
      </Button>
      <Button ref={keepRef} size="sm" variant="ghost" disabled={busy} onClick={keep}>
        Keep
      </Button>
    </div>
  );
}

function ScopeEditor({ client, groups }: { client: ServiceClient; groups: GroupRow[] }) {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const ids = useId();
  const save = async (scope: ScopeView) => {
    try {
      await unwrap(commands.setClientScope(client.client_id, scope));
      void qc.invalidateQueries({ queryKey: SERVICE_CLIENTS_KEY });
    } catch (e) {
      showError(e, () => void save(scope));
    }
  };
  const chosen = client.scope.mode === "groups" ? client.scope.group_ids : [];
  // Picking "Only some folder groups…" saves nothing until a group is ticked, so a misclick never
  // cuts an app off from every folder (M4 tester P2 finding).
  const [picking, setPicking] = useState(false);
  const mode = client.scope.mode === "groups" || picking ? "groups" : "all";
  return (
    <div className="space-y-2">
      <div className="grid items-center gap-2 min-[560px]:grid-cols-[minmax(8rem,12rem)_1fr_auto]">
        <span id={`${ids}-l`} className="truncate text-sm" title={client.name}>
          {client.name}
          {client.last_used_ms !== null && (
            <span className="block text-xs text-muted-foreground" title={absoluteTime(client.last_used_ms)}>
              Last used {relativeTime(client.last_used_ms)}
            </span>
          )}
        </span>
        <Select
          labelledBy={`${ids}-l`}
          value={mode}
          options={[
            { value: "all", label: "All my folders" },
            { value: "groups", label: "Only some folder groups…" },
          ]}
          placeholder="All my folders"
          onChange={(v) => {
            if (v === "all") {
              setPicking(false);
              if (client.scope.mode !== "all") void save({ mode: "all" });
            } else if (client.scope.mode !== "groups") {
              setPicking(true);
            }
          }}
        />
        {/* Only for apps with no open session (a running app is never cut off by surprise);
            HTTP tools have [Revoke…] instead. */}
        {client.kind !== "http" && client.sessions_open === 0 ? <ForgetApp client={client} labelId={`${ids}-l`} /> : <span />}
      </div>
      {mode === "groups" && (
        <div className="space-y-1.5 pl-1 min-[560px]:pl-[12.5rem]">
          {groups.map((g) => (
            <label key={g.id} className="flex items-center gap-2 text-sm">
              <input
                type="checkbox"
                className="size-4 shrink-0 accent-[var(--color-primary)]"
                checked={chosen.includes(g.id)}
                onChange={(e) =>
                  void save({
                    mode: "groups",
                    group_ids: e.target.checked ? [...chosen, g.id] : chosen.filter((x) => x !== g.id),
                  })
                }
              />
              {g.name}
            </label>
          ))}
          {chosen.length === 0 && (
            <p className="text-xs text-muted-foreground">
              {client.scope.mode === "groups"
                ? `${client.name} can't see any folders until you choose a group.`
                : `Choose at least one group. ${client.name} keeps seeing all your folders until you do.`}
            </p>
          )}
          <Link
            to="/settings/folder-groups"
            className="inline-block rounded-sm text-sm text-primary hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary"
          >
            Manage folder groups
          </Link>
        </div>
      )}
    </div>
  );
}

/** [Add a tool…] (OQ-45): name → a password shown once, masked on screen, full on [Copy]. */
function AddToolDialog({ onClose }: { onClose: () => void }) {
  const qc = useQueryClient();
  const ids = useId();
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [token, setToken] = useState<TokenView | null>(null);
  const create = async () => {
    setBusy(true);
    setError(null);
    try {
      setToken(await unwrap(commands.createToolToken(name)));
      void qc.invalidateQueries({ queryKey: SERVICE_CLIENTS_KEY });
    } catch (e) {
      setError(asUiError(e).body);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal
      open
      busy={busy}
      onClose={onClose}
      title="Add a tool"
      footer={
        token ? (
          <Button onClick={onClose}>Done</Button>
        ) : (
          <>
            <Button variant="ghost" disabled={busy} onClick={onClose}>
              Cancel
            </Button>
            <Button busy={busy} disabled={!name.trim()} onClick={() => void create()}>
              Create
            </Button>
          </>
        )
      }
    >
      <div className="space-y-1.5">
        <label htmlFor={`${ids}-n`} className="block text-sm font-medium">
          Name
        </label>
        <input
          id={`${ids}-n`}
          value={name}
          disabled={token !== null}
          maxLength={60}
          aria-invalid={error ? true : undefined}
          aria-describedby={error ? `${ids}-e` : undefined}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && name.trim() && !token) void create();
          }}
          className="h-9 w-full rounded-md border border-control-off bg-background px-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
        />
        {error && (
          <p id={`${ids}-e`} className="text-xs text-warning-strong">
            {error}
          </p>
        )}
      </div>
      {token && (
        <div className="space-y-2">
          <SnippetBlock shown={token.masked_snippet} copy={token.snippet} />
          <p className="flex items-center gap-1.5 text-sm">
            <KeyRound className="size-4 shrink-0 text-warning-strong" aria-hidden />
            This text includes a password for this tool. Keep it private.
          </p>
        </div>
      )}
    </Modal>
  );
}

function ToolRow({ tool }: { tool: ServiceClient }) {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const [confirm, setConfirm] = useState<"revoke" | "rotate" | null>(null);
  const [busy, setBusy] = useState(false);
  const [token, setToken] = useState<TokenView | null>(null);
  const keepRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (confirm) keepRef.current?.focus();
  }, [confirm]);
  const revoke = async () => {
    setBusy(true);
    try {
      await unwrap(commands.revokeTool(tool.client_id));
      void qc.invalidateQueries({ queryKey: SERVICE_CLIENTS_KEY });
    } catch (e) {
      showError(e, () => void revoke());
    } finally {
      setBusy(false);
      setConfirm(null);
    }
  };
  const rotate = async () => {
    setBusy(true);
    try {
      setToken(await unwrap(commands.rotateToolToken(tool.client_id)));
      void qc.invalidateQueries({ queryKey: SERVICE_CLIENTS_KEY });
    } catch (e) {
      showError(e, () => void rotate());
    } finally {
      setBusy(false);
      setConfirm(null);
    }
  };
  return (
    <div className="space-y-2 rounded-md border border-border p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="min-w-0 text-sm">
          <span className="block truncate font-medium">{tool.name}</span>
          <span className="block text-xs text-muted-foreground">
            {tool.token_last4 && (
              <>
                <span className="sr-only">Password hidden, ends in {tool.token_last4} · </span>
                <span aria-hidden>••••{tool.token_last4} · </span>
              </>
            )}
            {tool.last_used_ms !== null ? (
              <span title={absoluteTime(tool.last_used_ms)}>Last used {relativeTime(tool.last_used_ms)}</span>
            ) : (
              "Not used yet"
            )}
          </span>
        </span>
        {!confirm && (
          <span className="flex flex-wrap gap-2">
            <Button size="sm" variant="ghost" onClick={() => setConfirm("rotate")}>
              New setup text…
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setConfirm("revoke")}>
              Revoke…
            </Button>
          </span>
        )}
      </div>
      {confirm && (
        <div
          role="group"
          aria-label={confirm === "revoke" ? `Revoke ${tool.name}?` : `New setup text for ${tool.name}?`}
          className="flex flex-wrap items-center gap-2"
          onKeyDown={(e) => {
            if (e.key === "Escape") setConfirm(null);
          }}
        >
          <span className="text-sm">
            {confirm === "revoke"
              ? `${tool.name} can't connect any more. You can add it again later.`
              : `${tool.name}'s old setup text stops working. Paste the new one into it.`}
          </span>
          <Button size="sm" variant={confirm === "revoke" ? "danger" : "default"} busy={busy} onClick={() => void (confirm === "revoke" ? revoke() : rotate())}>
            {confirm === "revoke" ? "Revoke" : "Make new setup text"}
          </Button>
          <Button ref={keepRef} size="sm" variant="ghost" disabled={busy} onClick={() => setConfirm(null)}>
            Keep
          </Button>
        </div>
      )}
      {token && (
        <div className="space-y-2">
          <SnippetBlock shown={token.masked_snippet} copy={token.snippet} />
          <p className="flex items-center gap-1.5 text-sm">
            <KeyRound className="size-4 shrink-0 text-warning-strong" aria-hidden />
            This text includes a password for this tool. Keep it private.
          </p>
        </div>
      )}
    </div>
  );
}

function AdvancedApps() {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const policy = usePolicy();
  const ids = useId();
  const [adding, setAdding] = useState(false);
  const svc = useQuery({ queryKey: SERVICE_CLIENTS_KEY, queryFn: () => unwrap(commands.getServiceClients()) });
  const groups = useQuery({ queryKey: GROUPS_KEY, queryFn: () => unwrap(commands.getGroups()) }).data ?? [];
  const setHttp = async (on: boolean) => {
    try {
      const http = await unwrap(commands.setHttpEnabled(on));
      if (svc.data) qc.setQueryData(SERVICE_CLIENTS_KEY, { ...svc.data, http });
    } catch (e) {
      showError(e, () => void setHttp(on));
    }
  };
  if (svc.error) return <ErrorPanel error={asUiError(svc.error)} onRetry={() => void svc.refetch()} />;
  if (!svc.data) return null;
  const apps = svc.data.clients.filter((c) => c.kind !== "http");
  const tools = svc.data.clients.filter((c) => c.kind === "http");
  const http = svc.data.http;
  return (
    <div className="space-y-6">
      <section className="space-y-3" aria-labelledby="folder-access-h">
        <h3 id="folder-access-h" tabIndex={-1} className="text-sm font-semibold focus:outline-none">
          Folder access per app
        </h3>
        <p className="text-sm text-muted-foreground">
          For apps on this computer this is a privacy setting: those apps could read your files anyway.
        </p>
        {apps.length === 0 && (
          <p className="text-sm text-muted-foreground">Apps appear here after they use Selene Retrieval once.</p>
        )}
        {[...apps, ...tools].map((c) => (
          <ScopeEditor key={c.client_id} client={c} groups={groups} />
        ))}
      </section>
      <section className="space-y-3" aria-labelledby={`${ids}-http`}>
        <h3 id={`${ids}-http`} className="text-sm font-semibold">
          Local tools over HTTP
        </h3>
        {/* Page 29 uses "password" for the per-tool secret ("This text includes a password for this
            tool"); DS-7 lists "token" as jargon, so this line says "password" too (M4 tester P2). */}
        <SwitchRow
          label="Allow other local tools to connect over HTTP"
          help="127.0.0.1 only, password required"
          checked={http.enabled}
          managed={policy.field(PATHS.http)}
          onChange={(v) => void setHttp(v)}
        />
        {http.problem && <ErrorPanel error={http.problem} onRetry={() => void setHttp(true)} />}
        {http.enabled && http.url && (
          <div className="flex flex-wrap items-center justify-between gap-3">
            <p className="text-sm">
              Address: <code className="text-xs">{http.url}</code>
            </p>
            <Button size="sm" variant="outline" onClick={() => setAdding(true)}>
              Add a tool…
            </Button>
          </div>
        )}
        {tools.map((t) => (
          <ToolRow key={t.client_id} tool={t} />
        ))}
      </section>
      {adding && <AddToolDialog onClose={() => setAdding(false)} />}
    </div>
  );
}

export function ConnectedAppsPage() {
  const clients = useQuery({ queryKey: CLIENTS_KEY, queryFn: () => unwrap(commands.getClients()) });
  const ids = useId();
  const claude = clients.data?.apps.find((a) => a.app === "claude_desktop");
  const others = (clients.data?.apps ?? []).filter((a) => a.app !== "claude_desktop");
  return (
    <div className="space-y-4">
      <h1 className="text-lg font-semibold">Connected apps</h1>
      {clients.error && <ErrorPanel error={asUiError(clients.error)} onRetry={() => void clients.refetch()} />}
      <Card className="space-y-3" role="region" aria-labelledby={`${ids}-claude`}>
        <div className="flex flex-wrap items-baseline justify-between gap-2">
          <CardTitle id={`${ids}-claude`}>Claude Desktop</CardTitle>
          {claude?.state === "connected" && <LastUsed ms={claude.last_used_ms} />}
        </div>
        <ClaudeConnect manage />
      </Card>
      {others.map((a) => (
        <AppCard key={a.app} app={a} />
      ))}
      {clients.data && (
        <Card className="space-y-3" role="region" aria-labelledby={`${ids}-other`}>
          <CardTitle id={`${ids}-other`}>Other app</CardTitle>
          <CardText>Paste this setup text into your app's MCP settings, then restart that app.</CardText>
          <SnippetBlock shown={clients.data.other_snippet} />
        </Card>
      )}
      <FolderRequests />
      <Disclosure>
        <AdvancedApps />
      </Disclosure>
    </div>
  );
}
