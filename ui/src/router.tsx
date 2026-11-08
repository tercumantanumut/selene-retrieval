import { useEffect, useRef, useState } from "react";
import {
  Link,
  Outlet,
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  useNavigate,
  useRouterState,
} from "@tanstack/react-router";
import { Toaster } from "sonner";
import { commands, type UiError } from "@/bindings";
import { ApprovalCard } from "@/components/ApprovalCard";
import { ConsentHost } from "@/components/ConsentDialog";
import { ErrorPanel } from "@/components/ErrorPanel";
import { LiveRegion, useStateAnnouncements } from "@/components/LiveRegion";
import { MissingIndexPanel } from "@/components/MissingIndexPanel";
import { StatusPill } from "@/components/StatusPill";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { restartService, useAppView, useHasProvider, useLiveUpdates } from "@/lib/app";
import { requestSearchFocus } from "@/lib/store";
import { ActivityPage } from "@/pages/Activity";
import { ConnectedAppsPage } from "@/pages/ConnectedApps";
import { EnhancePage } from "@/pages/Enhance";
import { FolderDetailPage } from "@/pages/FolderDetail";
import { FolderGroupsPage } from "@/pages/FolderGroups";
import { FoldersPage } from "@/pages/Folders";
import { OverviewPage } from "@/pages/Overview";
import { ProvidersPage } from "@/pages/Providers";
import { SearchPage } from "@/pages/Search";
import { SettingsPage } from "@/pages/Settings";
import { WizardPage } from "@/pages/Wizard";
import { ChevronDown, Clock, KeyRound, Loader2, Menu, Sparkles } from "lucide-react";

type NavTo = "/" | "/folders" | "/search" | "/connected-apps" | "/settings" | "/enhance" | "/providers" | "/activity";

const linkClass =
  "whitespace-nowrap rounded-md px-3 py-1.5 text-sm hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background";

function NavLink({ to, label, exact = false }: { to: NavTo; label: string; exact?: boolean }) {
  return (
    <Link
      to={to}
      className={linkClass}
      activeProps={{ className: "bg-accent font-medium", "aria-current": "page" }}
      activeOptions={{ exact }}
    >
      {label}
    </Link>
  );
}

/** DS-4.17 M4: five top-level items, a "More" menu, and one "Menu" button below 760 px. */
function MainNav() {
  const navigate = useNavigate();
  const path = useRouterState({ select: (s) => s.location.pathname });
  const hasProvider = useHasProvider();
  const top: { to: NavTo; label: string; exact?: boolean }[] = [
    { to: "/", label: "Overview", exact: true },
    { to: "/folders", label: "Folders" },
    { to: "/search", label: "Search" },
    { to: "/connected-apps", label: "Connected apps" },
    { to: "/settings", label: "Settings" },
  ];
  // Prompt Enhancer only once a provider exists (D-21, U17).
  const more: { to: NavTo; label: string; icon: typeof Clock }[] = [
    ...(hasProvider ? [{ to: "/enhance" as const, label: "Prompt Enhancer", icon: Sparkles }] : []),
    { to: "/providers", label: "Providers & Keys", icon: KeyRound },
    { to: "/activity", label: "Activity", icon: Clock },
  ];
  const moreActive = more.some((m) => path.startsWith(m.to));
  return (
    <nav aria-label="Main" className="flex min-w-0 items-center gap-1">
      <div className="hidden items-center gap-1 min-[760px]:flex">
        {top.map((t) => (
          <NavLink key={t.to} {...t} />
        ))}
        <DropdownMenu>
          <DropdownMenuTrigger
            className={`${linkClass} inline-flex items-center gap-1 data-[state=open]:bg-accent ${moreActive ? "bg-accent font-medium" : ""}`}
          >
            More <ChevronDown className="size-4" aria-hidden />
          </DropdownMenuTrigger>
          <DropdownMenuContent>
            {more.map((m) => (
              <DropdownMenuItem key={m.to} onSelect={() => void navigate({ to: m.to })}>
                <m.icon className="text-muted-foreground" aria-hidden /> {m.label}
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
      <div className="min-[760px]:hidden">
        <DropdownMenu>
          <DropdownMenuTrigger className={`${linkClass} inline-flex items-center gap-1.5 data-[state=open]:bg-accent`}>
            <Menu className="size-4" aria-hidden /> Menu
          </DropdownMenuTrigger>
          <DropdownMenuContent>
            {top.map((t) => (
              <DropdownMenuItem key={t.to} onSelect={() => void navigate({ to: t.to })}>
                {t.label}
              </DropdownMenuItem>
            ))}
            <DropdownMenuSeparator />
            {more.map((m) => (
              <DropdownMenuItem key={m.to} onSelect={() => void navigate({ to: m.to })}>
                <m.icon className="text-muted-foreground" aria-hidden /> {m.label}
              </DropdownMenuItem>
            ))}
          </DropdownMenuContent>
        </DropdownMenu>
      </div>
    </nav>
  );
}

/** Page 04 keyboard shortcuts: Ctrl+K / Ctrl+F focus Search, Ctrl+, opens Settings. */
function useShortcuts(enabled: boolean) {
  const navigate = useNavigate();
  useEffect(() => {
    if (!enabled) return;
    const onKey = (e: KeyboardEvent) => {
      if (!e.ctrlKey || e.altKey || e.metaKey) return;
      // Inside a dialog the shortcuts would move the page behind it (focus stays trapped there).
      if (e.target instanceof Element && e.target.closest('[role="dialog"], [role="alertdialog"]')) return;
      const k = e.key.toLowerCase();
      if (k === "k" || k === "f") {
        e.preventDefault();
        void navigate({ to: "/search", search: {} }).then(() => requestSearchFocus());
      } else if (e.key === ",") {
        e.preventDefault();
        void navigate({ to: "/settings" });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [enabled, navigate]);
}

/**
 * Items already on screen when the window opens are announced as `status`; ones that arrive
 * later as `alert` (DS-4.7 change), so navigating never re-announces old notices.
 */
function useArrivedLater(ready: boolean, keys: string[]): (key: string) => boolean {
  const [first, setFirst] = useState<Set<string> | null>(null);
  if (first === null && ready) setFirst(new Set(keys)); // first loaded view (derived state)
  return (key) => first !== null && !first.has(key);
}

const noticeKey = (n: UiError) => `${n.title}|${n.details.split(" · ")[0]}`;

/**
 * WCAG 2.4.3 / DS-6 "focus is never lost": after a route change, focus moves to the new page's
 * `h1` (made focusable for this), unless the page already put focus somewhere inside the main
 * column itself (Search focuses its box, a folder's "View skipped files" its list heading).
 */
function useRouteFocus(path: string) {
  const first = useRef(true);
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    const main = document.getElementById("main");
    if (!main) return;
    const active = document.activeElement;
    if (active && active !== document.body && main.contains(active)) return;
    const h1 = main.querySelector<HTMLElement>("h1");
    if (h1) {
      if (!h1.hasAttribute("tabindex")) h1.tabIndex = -1;
      h1.focus({ preventScroll: true });
    }
  }, [path]);
}

function RootLayout() {
  useLiveUpdates();
  const view = useAppView();
  const path = useRouterState({ select: (s) => s.location.pathname });
  const inWizard = path === "/welcome";
  useShortcuts(!inWizard);
  // Keyed on the page actually rendered: `location` changes before the new page commits, and
  // focusing then would land on the outgoing page's h1 (lost to <body> when it unmounts).
  const shownPath = useRouterState({ select: (s) => s.matches.at(-1)?.pathname ?? s.location.pathname });
  useRouteFocus(shownPath);
  useStateAnnouncements(view);
  // R34: the service waits in its recovery mode; the choices replace the page (Settings stays
  // reachable for Help / Copy diagnostics).
  const missing = view?.index_missing && !view.restarting ? view.index_missing : null;
  const focusId = view?.focus_approval ?? null;
  const later = useArrivedLater(view !== undefined, [
    ...(view?.notices ?? []).map(noticeKey),
    ...(view?.approvals ?? []).map((a) => a.id),
  ]);
  // Folder requests appear on every page except the wizard; when the app was opened for one
  // (`--approve`, R-UX30) that card shows even there, so the request is never stuck unseen.
  const approvals = (view?.approvals ?? []).filter((a) => !inWizard || a.id === focusId);

  return (
    <div className="flex min-h-full flex-col">
      <header className="sticky top-0 z-10 flex items-center gap-4 border-b border-border bg-background px-6 py-3">
        {inWizard ? (
          <span className="shrink-0 font-semibold tracking-tight">Selene Retrieval</span>
        ) : (
          // M4: the brand links to Overview.
          <Link
            to="/"
            // Only the nav item marks the current page (one `aria-current` per page).
            activeProps={{ "aria-current": undefined }}
            className="shrink-0 rounded-sm font-semibold tracking-tight focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
          >
            Selene Retrieval
          </Link>
        )}
        {!inWizard && <MainNav />}
        <div className="ml-auto min-w-0">
          {view && <StatusPill overall={view.overall} summary={view.summary} />}
        </div>
      </header>
      <main id="main" className="mx-auto w-full max-w-3xl flex-1 space-y-4 px-6 py-6">
        {view?.restarting && (
          // Page 03 [M4]: in the problem panel's place while the service restarts.
          <p className="flex items-center gap-2 text-sm text-muted-foreground" role="status">
            <Loader2 className="size-4 animate-spin" aria-hidden /> Restarting the background service…
          </p>
        )}
        {view?.problem && (
          <ErrorPanel
            error={view.problem}
            onAction={
              view.problem.action.kind === "restart_app"
                ? () => void restartService()
                : view.problem.action.kind === "retry"
                  ? () => void commands.retryConnection()
                  : undefined
            }
          />
        )}
        {view?.notices.map((n) => (
          <ErrorPanel
            key={noticeKey(n)}
            error={n}
            live={later(noticeKey(n)) ? "alert" : "status"}
            onDismiss={() => void commands.dismissNotices()}
          />
        ))}
        {approvals.map((a) => (
          <ApprovalCard key={a.id} approval={a} focus={a.id === focusId} live={later(a.id) ? "alert" : "status"} />
        ))}
        {missing && <MissingIndexPanel missing={missing} />}
        {(!missing || path === "/settings") && <Outlet />}
      </main>
      <ConsentHost inWizard={inWizard} />
      <LiveRegion />
      <Toaster position="bottom-right" richColors closeButton />
    </div>
  );
}

const rootRoute = createRootRoute({ component: RootLayout });
const overviewRoute = createRoute({ getParentRoute: () => rootRoute, path: "/", component: OverviewPage });
const wizardRoute = createRoute({ getParentRoute: () => rootRoute, path: "/welcome", component: WizardPage });
const settingsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings",
  component: SettingsPage,
  // `section: "search"` = land on the Search quality card (page 31 [Change search model]);
  // `section: "answers"` = the Answers card (page 27 "Deeper answers is on/off").
  validateSearch: (s: Record<string, unknown>): { section?: "search" | "answers" } =>
    s.section === "search" || s.section === "answers" ? { section: s.section } : {},
});
const folderGroupsRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/settings/folder-groups",
  component: FolderGroupsPage,
});
const providersRoute = createRoute({ getParentRoute: () => rootRoute, path: "/providers", component: ProvidersPage });
const foldersRoute = createRoute({ getParentRoute: () => rootRoute, path: "/folders", component: FoldersPage });
const folderDetailRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/folders/$id",
  component: FolderDetailPage,
  // `section: "skipped"` = land on the Skipped files list ("View skipped files").
  validateSearch: (s: Record<string, unknown>): { section?: "skipped" } =>
    s.section === "skipped" ? { section: "skipped" } : {},
});
const searchRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/search",
  component: SearchPage,
  // The Overview box submits here; the query is never stored (page 26 privacy).
  validateSearch: (s: Record<string, unknown>): { q?: string } => (typeof s.q === "string" && s.q ? { q: s.q } : {}),
});
const enhanceRoute = createRoute({ getParentRoute: () => rootRoute, path: "/enhance", component: EnhancePage });
const connectedRoute = createRoute({
  getParentRoute: () => rootRoute,
  path: "/connected-apps",
  component: ConnectedAppsPage,
});
const activityRoute = createRoute({ getParentRoute: () => rootRoute, path: "/activity", component: ActivityPage });

// Memory history: a desktop window has no address bar, and a reload must not lose the route.
export const router = createRouter({
  routeTree: rootRoute.addChildren([
    overviewRoute,
    wizardRoute,
    settingsRoute,
    folderGroupsRoute,
    providersRoute,
    foldersRoute,
    folderDetailRoute,
    searchRoute,
    enhanceRoute,
    connectedRoute,
    activityRoute,
  ]),
  history: createMemoryHistory({ initialEntries: ["/"] }),
});

declare module "@tanstack/react-router" {
  interface Register {
    router: typeof router;
  }
}
