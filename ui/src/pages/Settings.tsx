import { useEffect, useId, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { save as saveDialog } from "@tauri-apps/plugin-dialog";
import { toast } from "sonner";
import { commands, type AdvancedView, type ModelChangeView, type TierName, type TierView, type UpdateInfo } from "@/bindings";
import { modelName } from "@/lib/format";
import { MoveIndexDialog } from "@/components/MoveIndexDialog";
import { Modal } from "@/components/ui/dialog";
import { ClaudeConnect } from "@/components/ClaudeConnect";
import { useProviderStatus } from "@/components/ConsentDialog";
import { ErrorPanel } from "@/components/ErrorPanel";
import { ManagedLine, ManagedNotice } from "@/components/Managed";
import { Progress } from "@/components/ui/progress";
import { Button } from "@/components/ui/button";
import { Card, CardText, CardTitle } from "@/components/ui/card";
import { Disclosure } from "@/components/ui/disclosure";
import { SwitchRow } from "@/components/ui/switch-row";
import {
  ABOUT_KEY,
  ADVANCED_KEY,
  ANSWERS_KEY,
  asUiError,
  CLAUDE_KEY,
  CONSENTS_KEY,
  copyDiagnostics,
  restartService,
  toastRestartNeeded,
  unwrap,
  useAppView,
  useClaudeStatus,
  useErrorToast,
} from "@/lib/app";
import { PATHS, usePolicy } from "@/lib/policy";
import { askConsent } from "@/lib/store";
import { isMac, OS_NAME, START_AT_LOGIN, UNINSTALL_HINT } from "@/lib/platform";

const TIER_RANK = { basic: 0, better: 1, best: 2 } as const;
const TIER_WORD = { basic: "Basic search", better: "Better search", best: "Best search" } as const;

/** Download size of the recommended model, in plain words (02 §2.5 "Recommended for your computer"). */
function downloadSize(t: TierView): string {
  if (t.recommended === "best") return "638 MB";
  return t.gpu ? "208 MB" : "105 MB";
}

/** Progress of a search model change: pushed events only (download → improving → done). */
function ModelChange({ change }: { change: ModelChangeView }) {
  const showError = useErrorToast();
  const run = (allowMetered: boolean) =>
    unwrap(commands.useRecommendedModel(allowMetered)).catch((e) => showError(e, () => void run(allowMetered)));
  const stop = () => unwrap(commands.cancelModelChange()).catch((e) => showError(e));
  const pct = change.total > 0 ? Math.round((100 * Math.min(change.done, change.total)) / change.total) : null;
  if (change.step === "failed" && change.error) {
    return <ErrorPanel error={change.error} onRetry={() => void run(false)} />;
  }
  return (
    <div className="space-y-2">
      <p className="text-sm">{change.text}</p>
      {(change.step === "downloading" || change.step === "improving") && (
        // Named and valued (DS-4.5); the live region announces each step once, not each tick.
        <Progress value={pct} label="Search model change" valueText={change.text} />
      )}
      <div className="flex flex-wrap gap-2">
        {change.step === "waiting_for_unmetered" && (
          <Button size="sm" onClick={() => void run(true)}>
            Download anyway
          </Button>
        )}
        {(change.step === "downloading" || change.step === "improving" || change.step === "waiting_for_unmetered") && (
          <Button size="sm" variant="ghost" onClick={() => void stop()}>
            Stop
          </Button>
        )}
      </div>
    </div>
  );
}

/**
 * "Recommended for your computer" line (02 §2.5): shown only when a better tier is recommended.
 * [Use it] is the consent to download; nothing downloads without that click. When the
 * organisation turned model downloads off, the offer is hidden here (page 33: blocked features
 * are hidden in Basic; Advanced shows them locked).
 */
function RecommendedLine({ tier, change }: { tier: TierView | null; change: ModelChangeView | null }) {
  const showError = useErrorToast();
  const policy = usePolicy();
  if (change && change.step !== "done") return <ModelChange change={change} />;
  const blocked = policy.field(PATHS.modelDownloads, PATHS.searchModel) !== null;
  if (!tier || blocked || TIER_RANK[tier.recommended] <= TIER_RANK[tier.active]) {
    return <CardText>{change?.text ?? "Search is set up for this computer."}</CardText>;
  }
  const startUpgrade = () =>
    unwrap(commands.useRecommendedModel(false)).catch((e) => showError(e, () => void startUpgrade()));
  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-center gap-3">
        <p className="text-sm">
          Recommended for your computer: <span className="font-medium">{TIER_WORD[tier.recommended]}</span> (
          {downloadSize(tier)} download)
        </p>
        <Button size="sm" variant="outline" onClick={() => void startUpgrade()}>
          Use it
        </Button>
      </div>
      <CardText>Downloads once from the model's official page. Search keeps working while it improves.</CardText>
    </div>
  );
}

const TIER_SIZE: Record<TierName, string> = { basic: "built in", better: "105–208 MB download", best: "638 MB download" };
/** Multilingual model download (M3-Q pins): fp16 file with a graphics card, fp32 without. */
const multilingualSize = (tier: TierView | null) => (tier?.gpu ? "236 MB" : "471 MB");
/** Page 12 [M3, OQ-22]: the offer shows only when the Windows display language isn't English. */
const windowsNotEnglish = () => !(navigator.languages?.[0] ?? navigator.language ?? "en").toLowerCase().startsWith("en");

async function chooseMultilingual(showError: (e: unknown, retry?: () => void) => void) {
  try {
    await unwrap(commands.chooseMultilingualSearch());
    toast.info("Switching to Multilingual search", {
      description: "Search keeps working while it changes. Progress is shown under Search quality.",
    });
  } catch (e) {
    showError(e, () => void chooseMultilingual(showError));
  }
}

/** Page 12 multilingual row: one line + [Use it], never a modal. Hidden when downloads are managed. */
function MultilingualOffer({ tier, change }: { tier: TierView | null; change: ModelChangeView | null }) {
  const showError = useErrorToast();
  const policy = usePolicy();
  if (!tier || tier.multilingual || change || !windowsNotEnglish()) return null;
  if (policy.field(PATHS.modelDownloads, PATHS.searchModel)) return null;
  return (
    <div className="flex flex-wrap items-center justify-between gap-3 border-t border-border pt-3">
      <CardText>
        Your {isMac ? "Mac's" : "Windows"} language isn't English. Search in many languages ({multilingualSize(tier)} download)
      </CardText>
      <Button size="sm" variant="outline" onClick={() => void chooseMultilingual(showError)}>
        Use it
      </Button>
    </div>
  );
}

/** Page 13 · Answers: extra search rounds for deeper answers (1–5). Advanced only, with a provider. */
function DeeperRounds() {
  const status = useProviderStatus();
  const qc = useQueryClient();
  const showError = useErrorToast();
  const policy = usePolicy();
  const ids = useId();
  const has = Boolean(status.data?.provider);
  const a = useQuery({ queryKey: ANSWERS_KEY, queryFn: () => unwrap(commands.getAnswersSettings()), enabled: has }).data;
  if (!has || !a) return null;
  const managed = policy.field(PATHS.maxRounds);
  const save = async (n: number) => {
    try {
      qc.setQueryData(ANSWERS_KEY, await unwrap(commands.setAnswersSettings(null, null, n)));
    } catch (e) {
      showError(e, () => void save(n));
    }
  };
  return (
    <div className="flex items-center justify-between gap-4">
      <div className="min-w-0">
        <label htmlFor={`${ids}-n`}>Extra search rounds for deeper answers</label>
        {managed ? (
          <ManagedLine field={managed} id={`${ids}-h`} />
        ) : (
          <span id={`${ids}-h`} className="block text-muted-foreground">
            1 to 5. Used only when Deeper answers is on. More rounds use more AI credits.
          </span>
        )}
      </div>
      <input
        id={`${ids}-n`}
        type="number"
        min={1}
        max={5}
        step={1}
        value={a.max_rounds}
        readOnly={managed !== null}
        aria-disabled={managed !== null || undefined}
        aria-describedby={`${ids}-h`}
        onChange={(e) => {
          if (managed) return;
          const n = Math.round(Number(e.target.value));
          if (n >= 1 && n <= 5 && n !== a.max_rounds) void save(n);
        }}
        className="h-9 w-16 rounded-md border border-control-off bg-background px-2 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background aria-disabled:cursor-not-allowed"
      />
    </div>
  );
}

/** Advanced switches (02 §2.5). Basic use never needs them; secure defaults stay on. */
function AdvancedSwitches({ tier }: { tier: TierView | null }) {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const policy = usePolicy();
  const ids = useId();
  const adv = useQuery({ queryKey: ADVANCED_KEY, queryFn: () => unwrap(commands.getAdvancedSettings()) }).data;
  const set = async (gpu: boolean | null, requests: boolean | null, checks: boolean | null = null) => {
    try {
      const next: AdvancedView = await unwrap(commands.setAdvancedSettings(gpu, requests, checks));
      qc.setQueryData(ADVANCED_KEY, next);
      // OQ-24 (M4): the toast carries [Restart now]; the Advanced footer offers it too.
      if (next.restart_needed) toastRestartNeeded();
    } catch (e) {
      showError(e, () => void set(gpu, requests, checks));
    }
  };
  const choose = async (t: TierName) => {
    try {
      await unwrap(commands.chooseSearchTier(t));
      toast.info(t === "basic" ? "Switching to Basic search" : `Switching to ${TIER_WORD[t]}`, {
        description: "Search keeps working while it changes. Progress is shown under Search quality.",
      });
    } catch (e) {
      showError(e, () => void choose(t));
    }
  };
  if (!adv) return null;
  const above = (t: TierName) => tier !== null && TIER_RANK[t] > TIER_RANK[tier.recommended];
  // Page 33: a managed search model locks every choice; managed downloads lock the ones that
  // download (Basic is built in and stays available).
  const modelLock = policy.field(PATHS.searchModel);
  const downloadLock = modelLock ?? policy.field(PATHS.modelDownloads);
  const lockFor = (t: TierName | "multilingual") => (t === "basic" ? modelLock : downloadLock);
  const anyLock = downloadLock !== null;
  return (
    <div className="space-y-4">
      <SwitchRow
        label="Use graphics card"
        help="Indexes faster. Turn off if other apps slow down while indexing. Applies after a restart."
        checked={adv.use_graphics_card}
        managed={policy.field(PATHS.useGraphicsCard)}
        onChange={(v) => void set(v, null)}
      />
      <div className="space-y-2" role="group" aria-labelledby={`${ids}-model`}>
        <span id={`${ids}-model`} className="block">
          Search model
        </span>
        <div className="flex flex-wrap gap-2">
          {(["basic", "better", "best"] as const).map((t) => {
            const active = tier?.active === t && !tier.multilingual;
            const lock = lockFor(t);
            return (
              <Button
                key={t}
                size="sm"
                variant={active ? "default" : "outline"}
                // The active model is shown pressed (not disabled, so it stays readable and
                // reachable); choosing it again does nothing.
                aria-pressed={active}
                locked={lock !== null}
                aria-describedby={lock ? `${ids}-lock` : undefined}
                onClick={() => {
                  if (!active) void choose(t);
                }}
                title={TIER_SIZE[t]}
              >
                {TIER_WORD[t]}
              </Button>
            );
          })}
          <Button
            size="sm"
            variant={tier?.multilingual ? "default" : "outline"}
            aria-pressed={tier?.multilingual ?? false}
            locked={lockFor("multilingual") !== null}
            aria-describedby={anyLock ? `${ids}-lock` : undefined}
            onClick={() => {
              if (!tier?.multilingual) void chooseMultilingual(showError);
            }}
            title={`For folders in several languages · ${multilingualSize(tier)} download`}
          >
            Multilingual search
          </Button>
        </div>
        {downloadLock && <ManagedLine field={downloadLock} id={`${ids}-lock`} />}
        {!anyLock && tier && (["better", "best"] as const).some(above) && (
          <p className="text-muted-foreground">
            Levels above the recommendation may index slowly on this computer. Search keeps working while it runs.
          </p>
        )}
      </div>
      <SwitchRow
        label="Let connected apps ask to add folders"
        help="Off by default. When on, Claude can ask (restart Claude to see it); nothing is added until you allow it here."
        checked={adv.allow_app_folder_requests}
        managed={policy.field(PATHS.folderRequests)}
        onChange={(v) => void set(null, v)}
      />
      <SwitchRow
        label="Let connected apps check folders for changes"
        help="Off by default. When on, connected apps can ask for a check (restart them to see it)."
        checked={adv.allow_app_folder_checks}
        managed={policy.field(PATHS.folderChecks)}
        onChange={(v) => void set(null, null, v)}
      />
    </div>
  );
}

/** Page 11 [M3] AI provider summary; the full controls live on Providers & Keys (page 21). */
function ProviderSummary() {
  const status = useProviderStatus();
  const navigate = useNavigate();
  const policy = usePolicy();
  const st = status.data;
  // Page 33: cloud providers or the provider list may be limited; local search is never affected.
  const managed = policy.field(PATHS.remoteProviders, PATHS.allowedProviders);
  return (
    <Card className="space-y-3">
      <CardTitle>AI provider</CardTitle>
      {status.error && <ErrorPanel error={asUiError(status.error)} onRetry={() => void status.refetch()} />}
      {st && (
        <div className="flex flex-wrap items-center justify-between gap-3">
          <CardText>
            {st.provider
              ? `${st.provider_name ?? st.provider} · answers use ${modelName(st.models, st.model ?? st.default_model) ?? "the automatic model"}`
              : "No AI provider. Search works without one."}
          </CardText>
          <Button size="sm" variant="outline" onClick={() => void navigate({ to: "/providers" })}>
            {st.provider ? "Change" : "Add a provider"}
          </Button>
        </div>
      )}
      {managed && <ManagedLine field={managed} />}
    </Card>
  );
}

/**
 * Page 23 · Answers (only once a provider exists, D-21). Deeper answers stay off until the user
 * turns them on (P1.2, U26). Turning answers on without the consent opens the consent dialog.
 * `focus` (the "Deeper answers is on/off" link on an answer): the card's title takes focus.
 */
function AnswersCard({ focus }: { focus: boolean }) {
  const status = useProviderStatus();
  const qc = useQueryClient();
  const showError = useErrorToast();
  const policy = usePolicy();
  const ids = useId();
  const headingRef = useRef<HTMLHeadingElement>(null);
  const st = status.data;
  const hasProvider = Boolean(st?.provider);
  const answers = useQuery({
    queryKey: ANSWERS_KEY,
    queryFn: () => unwrap(commands.getAnswersSettings()),
    enabled: hasProvider,
  });
  const consents = useQuery({
    queryKey: CONSENTS_KEY,
    queryFn: () => unwrap(commands.getConsents()),
    enabled: hasProvider,
  });
  const ready = hasProvider && answers.data !== undefined;
  useEffect(() => {
    if (focus && ready) {
      headingRef.current?.scrollIntoView({ block: "start" });
      headingRef.current?.focus();
    }
  }, [focus, ready]);
  if (!hasProvider) return null;
  const a = answers.data;
  const provider = st?.provider_name ?? st?.provider ?? "";
  const enabledLock = policy.field(PATHS.answersEnabled);
  const deeperLock = policy.field(PATHS.deeperAnswers);
  // Page 33 "per section": every field of the card managed → one line under the title.
  const sectionLock = enabledLock && deeperLock ? enabledLock : null;

  const save = async (enabled: boolean | null, deeper: boolean | null) => {
    try {
      qc.setQueryData(ANSWERS_KEY, await unwrap(commands.setAnswersSettings(enabled, deeper, null)));
    } catch (e) {
      showError(e, () => void save(enabled, deeper));
    }
  };
  const setAnswers = (on: boolean) => {
    const consented = consents.data?.find((c) => c.purpose === "answer")?.allowed ?? false;
    if (on && !consented) {
      askConsent({
        purpose: "answer",
        providerName: provider,
        onDone: (ok) => {
          if (ok) void save(true, null);
        },
      });
      return;
    }
    void save(on, null);
  };

  return (
    <Card className="space-y-4">
      <h2 ref={headingRef} tabIndex={-1} className="text-base font-semibold leading-tight focus:outline-none">
        Answers
      </h2>
      {sectionLock && <ManagedLine field={sectionLock} id={`${ids}-sec`} />}
      {answers.error && <ErrorPanel error={asUiError(answers.error)} onRetry={() => void answers.refetch()} />}
      {a && (
        <>
          <SwitchRow
            label="Let connected apps ask for answers written from your files"
            // M4 tester P2: the same switch also turns on [Ask] on the Search page; say so.
            help={`Also turns on Ask on the Search page. Uses your ${provider} account. Search works without it.`}
            checked={a.answers_enabled}
            managed={sectionLock ? null : enabledLock}
            lockedBy={sectionLock ? `${ids}-sec` : undefined}
            onChange={setAnswers}
          />
          <SwitchRow
            label="Deeper answers (uses more AI credits, slower)"
            help={`Searches again up to ${a.max_rounds} more times when the first answer isn't enough.`}
            checked={a.deeper_answers}
            managed={sectionLock ? null : deeperLock}
            lockedBy={sectionLock ? `${ids}-sec` : undefined}
            blockedHelp={sectionLock || a.answers_enabled ? undefined : "Turn on answers first."}
            onChange={(v) => void save(null, v)}
          />
        </>
      )}
    </Card>
  );
}

const KEYS: [string, string][] = [
  ["Ctrl+K or Ctrl+F", "Search your folders"],
  ["Ctrl+,", "Open Settings"],
];

/** "View licences" (page 18): the texts bundled with the app, in a scrollable dialog. */
function LicencesDialog({ onClose }: { onClose: () => void }) {
  const list = useQuery({ queryKey: ["licenses"], queryFn: () => commands.getLicenses() }).data;
  return (
    <Modal open onClose={onClose} title="Licences" footer={<Button onClick={onClose}>Close</Button>}>
      <div
        tabIndex={0}
        role="region"
        aria-label="Licence texts"
        className="max-h-[60vh] space-y-4 overflow-y-auto rounded-md border border-border p-3 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary"
      >
        {list && list.length === 0 && (
          <p className="text-sm text-muted-foreground">The licence texts are installed with Selene Retrieval.</p>
        )}
        {list?.map((l) => (
          <section key={l.name} aria-label={l.name} className="space-y-1">
            <h3 className="text-sm font-semibold">{l.name}</h3>
            <pre className="whitespace-pre-wrap break-words font-mono text-xs">{l.text}</pre>
          </section>
        ))}
      </div>
    </Modal>
  );
}

/** Page 18 [M4] About: version, search model, graphics card, shortcuts, licences, uninstall. */
function About({ version, tier }: { version: string; tier: TierView | null }) {
  const [licences, setLicences] = useState(false);
  return (
    <div className="space-y-2 border-t border-border pt-3 text-sm">
      <p className="font-medium">Selene Retrieval {version}</p>
      {tier && <p>Search model: {tier.multilingual ? "Multilingual search" : TIER_WORD[tier.active]}</p>}
      {tier && <p>Graphics card: {tier.gpu ? "used" : "not used"}</p>}
      <div>
        <p className="text-muted-foreground">Keyboard shortcuts</p>
        <dl className="mt-1 grid grid-cols-[auto_1fr] gap-x-4 gap-y-0.5">
          {KEYS.map(([k, v]) => (
            <div key={k} className="contents">
              <dt>
                <kbd className="rounded border border-border bg-muted px-1.5 text-xs">{k}</kbd>
              </dt>
              <dd className="text-muted-foreground">{v}</dd>
            </div>
          ))}
        </dl>
      </div>
      <Button size="sm" variant="outline" onClick={() => setLicences(true)}>
        View licences
      </Button>
      <p className="text-muted-foreground">{UNINSTALL_HINT}</p>
      {licences && <LicencesDialog onClose={() => setLicences(false)} />}
    </div>
  );
}

export function SettingsPage() {
  const view = useAppView();
  const qc = useQueryClient();
  const showError = useErrorToast();
  const policy = usePolicy();
  const ids = useId();
  const about = useQuery({ queryKey: ABOUT_KEY, queryFn: () => commands.getAbout() }).data;
  const [update, setUpdate] = useState<UpdateInfo | null | undefined>(undefined);
  const [checking, setChecking] = useState(false);
  const [moving, setMoving] = useState(false);
  const claude = useClaudeStatus();
  const adv = useQuery({ queryKey: ADVANCED_KEY, queryFn: () => unwrap(commands.getAdvancedSettings()) }).data;
  // Page 31 [Change search model] lands on Search quality (one search model for every folder
  // group, contract §3); "Deeper answers is on/off" (page 27) lands on Answers.
  const { section } = useSearch({ from: "/settings" });
  const searchHeading = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    if (section === "search") {
      searchHeading.current?.scrollIntoView({ block: "start" });
      searchHeading.current?.focus();
    }
  }, [section]);
  const dataDirLock = policy.field(PATHS.dataDir);

  /** OQ-30: the same redacted text as "Copy diagnostics", into a `.txt` file the user picks. */
  const saveDiagnosticsFile = async () => {
    const path = await saveDialog({
      title: "Save diagnostics",
      defaultPath: "Selene Retrieval diagnostics.txt",
      filters: [{ name: "Text", extensions: ["txt"] }],
    });
    if (!path) return;
    try {
      await unwrap(commands.saveDiagnostics(path));
      toast.success("Diagnostics saved.");
    } catch (e) {
      showError(e, () => void saveDiagnosticsFile());
    }
  };

  const toggleStart = async (enabled: boolean) => {
    try {
      const now = await unwrap(commands.setStartWithWindows(enabled));
      if (about) qc.setQueryData(ABOUT_KEY, { ...about, start_with_windows: now, autostart_decided: true });
    } catch (e) {
      showError(e, () => void toggleStart(enabled));
    }
  };

  const checkUpdate = async () => {
    setChecking(true);
    try {
      setUpdate(await unwrap(commands.checkForUpdate()));
    } catch (e) {
      showError(e, () => void checkUpdate());
    } finally {
      setChecking(false);
    }
  };

  const installUpdate = async () => {
    toast.info("Updating", { description: "Selene Retrieval will restart when the update is ready." });
    try {
      await unwrap(commands.installUpdate());
    } catch (e) {
      showError(e, () => void installUpdate());
    }
  };

  const disconnect = async () => {
    try {
      const r = await unwrap(commands.disconnectClaude());
      qc.setQueryData(CLAUDE_KEY, r.status);
      toast.success(r.message);
    } catch (e) {
      showError(e);
    }
  };

  return (
    <div className="space-y-4">
      <h1 className="text-lg font-semibold">Settings</h1>
      {policy.any && <ManagedNotice />}

      <Card className="space-y-3">
        <CardTitle>General</CardTitle>
        {/* Rendered only once the real state is known, so it never flashes "off". */}
        {about && (
          <SwitchRow
            label={START_AT_LOGIN}
            help="Keeps your folders up to date in the background."
            checked={about.start_with_windows}
            managed={policy.field(PATHS.startWithWindows)}
            onChange={(v) => void toggleStart(v)}
          />
        )}
      </Card>

      <Card className="space-y-3">
        <h2 ref={searchHeading} tabIndex={-1} className="text-base font-semibold leading-tight focus:outline-none">
          Search quality
        </h2>
        <RecommendedLine tier={view?.tier ?? null} change={view?.model_change ?? null} />
        <MultilingualOffer tier={view?.tier ?? null} change={view?.model_change ?? null} />
      </Card>

      <ProviderSummary />
      <AnswersCard focus={section === "answers"} />

      <Card className="space-y-3">
        <CardTitle>Connected apps</CardTitle>
        <ClaudeConnect />
        <Button asChild size="sm" variant="outline">
          <Link to="/connected-apps">Manage connected apps</Link>
        </Button>
      </Card>

      <Card className="space-y-3">
        <CardTitle>Help</CardTitle>
        <div className="flex flex-wrap items-center gap-3">
          <Button variant="outline" onClick={() => void copyDiagnostics()}>
            Copy diagnostics
          </Button>
          <Button variant="outline" onClick={() => void saveDiagnosticsFile()}>
            Save as file…
          </Button>
          <Button variant="outline" onClick={() => void checkUpdate()} busy={checking}>
            Check for updates
          </Button>
          <span aria-live="polite">
            {update === null && <span className="text-sm text-muted-foreground">You have the latest version.</span>}
          </span>
          {update && (
            <Button onClick={() => void installUpdate()}>Install version {update.version}</Button>
          )}
        </div>
        {view?.reduced_protection && (
          <CardText>File reader running with reduced protection</CardText>
        )}
        <CardText>Diagnostics never include your files, searches, folder names or keys.</CardText>
        <details className="text-sm">
          <summary className="cursor-pointer select-none rounded-sm text-muted-foreground">Show what's included</summary>
          <ul className="mt-1 list-disc space-y-0.5 pl-5 text-muted-foreground">
            <li>App version and {OS_NAME} version</li>
            <li>Background service state</li>
            <li>Folder counts</li>
            <li>Claude connection state</li>
            <li>Recent error codes</li>
            <li>Never: file names, folder names, searches, keys</li>
          </ul>
        </details>
        <About version={about?.version ?? ""} tier={view?.tier ?? null} />
      </Card>

      {/* Page 31 [Change search model] opens Advanced too: the model choices live there. */}
      <Disclosure defaultOpen={section === "search"} contentClassName="space-y-3 text-sm">
        <div className="flex flex-wrap items-center gap-3">
          <p className="min-w-0 flex-1">
            <span className="text-muted-foreground">Index location: </span>
            <code className="break-all">{about?.data_dir}</code>
            {dataDirLock && <ManagedLine field={dataDirLock} id={`${ids}-dir`} />}
          </p>
          <Button
            size="sm"
            variant="outline"
            locked={dataDirLock !== null}
            aria-describedby={dataDirLock ? `${ids}-dir` : undefined}
            onClick={() => setMoving(true)}
          >
            Move…
          </Button>
        </div>
        <AdvancedSwitches tier={view?.tier ?? null} />
        <DeeperRounds />
        <p>
          <Link
            to="/settings/folder-groups"
            className="rounded-sm text-primary underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
          >
            Folder groups
          </Link>
          <span className="text-muted-foreground"> · Give apps access to only some folders.</span>
        </p>
        <p>
          <span className="text-muted-foreground">Version: </span>
          {about?.version}
        </p>
        {adv?.restart_needed && (
          // OQ-24: the same action as the toast, while a restart is pending.
          <p className="flex flex-wrap items-center gap-3">
            <span>This takes effect the next time Selene Retrieval starts.</span>
            <Button size="sm" onClick={() => void restartService()}>
              Restart now
            </Button>
          </p>
        )}
        <div className="flex flex-wrap gap-3 pt-1">
          <Button size="sm" variant="outline" onClick={() => void commands.retryConnection()}>
            Reconnect to the background service
          </Button>
          {claude?.claude === "connected" && (
            // X19: only offered when Claude is actually connected.
            <Button size="sm" variant="ghost" onClick={() => void disconnect()}>
              Disconnect Claude Desktop
            </Button>
          )}
        </div>
        {moving && <MoveIndexDialog onClose={() => setMoving(false)} />}
      </Disclosure>
    </div>
  );
}
