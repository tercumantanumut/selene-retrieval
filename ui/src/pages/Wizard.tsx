// First-run wizard (P1.3): pick a folder → pre-scan estimate → indexing starts automatically with
// live (pushed) progress → optional AI provider (M3, page 20; skipping keeps local search) →
// connect Claude Desktop or copy the setup text.
// Amendment 13: step 1 also offers [Create a folder for me] (one click: the app makes
// `Documents\Selene Retrieval`, adds it and opens it); step 2 then says it's ready, and Finish opens
// that folder's details.
import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "@tanstack/react-router";
import { commands, type DefaultFolderResult, type Estimate, type UiError } from "@/bindings";
import { ClaudeConnect } from "@/components/ClaudeConnect";
import { ErrorPanel } from "@/components/ErrorPanel";
import { FolderRootsHint } from "@/components/Managed";
import { FolderCard } from "@/components/FolderCard";
import { CheckAndSaveButton, ProviderForm } from "@/components/ProviderForm";
import { Button } from "@/components/ui/button";
import { Card, CardText } from "@/components/ui/card";
import { ABOUT_KEY, asUiError, unwrap, useAppView, useErrorToast } from "@/lib/app";
import { defaultFolderHint, useCreateDefaultFolder, useDefaultFolder } from "@/lib/defaultFolder";
import { baseName, pickFolder } from "@/lib/folders";
import { cn } from "@/lib/utils";
import { Cloud, FolderOpen, FolderPlus, FolderSearch, Loader2 } from "lucide-react";

type Step =
  | { kind: "pick" }
  | { kind: "counting"; path: string }
  | { kind: "big"; path: string; est: Estimate }
  | { kind: "adding"; path: string; est: Estimate }
  | { kind: "progress"; path: string; est: Estimate; folderId: string }
  | { kind: "ready"; res: DefaultFolderResult }
  | { kind: "provider" }
  | { kind: "connect" };

const STEPS = 4;
const STEP_NO: Record<Step["kind"], number> = {
  pick: 1,
  counting: 1,
  big: 1,
  adding: 2,
  progress: 2,
  ready: 2,
  provider: 3,
  connect: 4,
};

/**
 * The step's title is the page's `h1` (DS-6: one per page). When the step changes, focus moves
 * to it (DS-6 "focus is never lost": the button that was pressed has gone), so screen-reader
 * users hear where they are; step 2 focuses [Next] instead (page 06).
 */
function StepTitle({ children, focus }: { children: React.ReactNode; focus: boolean }) {
  const ref = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    if (focus) ref.current?.focus();
  }, [focus]);
  return (
    <h1 ref={ref} tabIndex={-1} className="text-xl font-semibold leading-tight focus:outline-none">
      {children}
    </h1>
  );
}

/** DS-4.14 (OQ-12): "Step n of 4" with dots; the text is the accessible label. */
function Stepper({ n }: { n: number }) {
  return (
    <div className="flex items-center gap-3">
      <span className="flex gap-1.5" aria-hidden>
        {Array.from({ length: STEPS }, (_, i) => (
          <span
            key={i}
            data-status-dot={i < n ? "" : undefined}
            className={cn(
              "size-2 rounded-full",
              i < n ? "bg-primary" : "border border-control-off bg-transparent",
            )}
          />
        ))}
      </span>
      <p className="text-sm text-muted-foreground" aria-current="step">
        Step {n} of {STEPS}
      </p>
    </div>
  );
}

export function WizardPage() {
  const [step, setStep] = useState<Step>({ kind: "pick" });
  const [error, setError] = useState<{ err: UiError; retry?: () => void } | null>(null);
  const [startWithWindows, setStartWithWindows] = useState<boolean | null>(null);
  const [afterFinish, setAfterFinish] = useState<"/" | "/providers">("/");
  // Amendment 13: the folder made by [Create a folder for me]; Finish opens its details.
  const [createdId, setCreatedId] = useState<string | null>(null);
  const offer = useDefaultFolder();
  const maker = useCreateDefaultFolder();
  const view = useAppView();
  const qc = useQueryClient();
  const navigate = useNavigate();
  const showError = useErrorToast();
  const about = useQuery({ queryKey: ABOUT_KEY, queryFn: () => commands.getAbout() }).data;
  const nextRef = useRef<HTMLButtonElement>(null);
  // Focus the new step's title only after the user moved on (not on first load).
  const [moved, setMoved] = useState(false);
  const [lastKind, setLastKind] = useState(step.kind);
  if (lastKind !== step.kind) {
    // Derived state (React docs pattern): a step change since the first render.
    setLastKind(step.kind);
    if (!moved) setMoved(true);
  }

  // OQ-16: a user who already chose (e.g. turned "Start with Windows" off, then removed every
  // folder) sees their current setting; only a real first run starts with it checked.
  const autostartChecked = startWithWindows ?? (about ? (about.autostart_decided ? about.start_with_windows : true) : true);

  // Page 06 [New]: focus moves to [Next] once it becomes enabled.
  useEffect(() => {
    if (step.kind === "progress" || step.kind === "ready") nextRef.current?.focus();
  }, [step.kind]);

  // [Create a folder for me]: one click; any failure falls back to choosing a folder.
  const createFolder = async () => {
    setError(null);
    const res = await maker.create();
    if (res) {
      setCreatedId(res.folder_id);
      setStep({ kind: "ready", res });
    }
  };

  const add = async (path: string, est: Estimate) => {
    setError(null);
    setStep({ kind: "adding", path, est });
    try {
      const folderId = await unwrap(commands.addFolder(path));
      setStep({ kind: "progress", path, est, folderId });
    } catch (e) {
      setStep({ kind: "pick" });
      setError({ err: asUiError(e), retry: () => void add(path, est) });
    }
  };

  const choose = async (defaultPath?: string) => {
    setError(null);
    maker.clearError();
    const path = await pickFolder(defaultPath);
    if (!path) return;
    setStep({ kind: "counting", path });
    try {
      const est = await unwrap(commands.estimateFolder(path));
      if (est.capped && est.files >= 500_000) setStep({ kind: "big", path, est });
      else await add(path, est);
    } catch (e) {
      setStep({ kind: "pick" });
      setError({ err: asUiError(e), retry: () => void choose() });
    }
  };

  const finish = async () => {
    // Write the setting only on a real first run or when the user changed the box here.
    const current = about?.start_with_windows ?? false;
    if (!about?.autostart_decided || autostartChecked !== current) {
      try {
        const now = await unwrap(commands.setStartWithWindows(autostartChecked));
        if (about) qc.setQueryData(ABOUT_KEY, { ...about, start_with_windows: now, autostart_decided: true });
      } catch (e) {
        showError(e);
      }
    }
    if (createdId && afterFinish === "/") void navigate({ to: "/folders/$id", params: { id: createdId }, search: {} });
    else void navigate({ to: afterFinish });
  };

  const openFolder = (id: string) => {
    unwrap(commands.openFolder(id)).catch((e) => showError(e));
  };

  const shownError = error?.err ?? maker.error;
  const onErrorAction = () => {
    if (!shownError) return;
    const a = shownError.action;
    maker.clearError();
    if (a.kind === "choose_subfolder") void choose(a.path);
    else if (a.kind === "choose_folder") void choose();
    else if (error) error.retry?.();
    else void createFolder();
  };
  const offered = offer?.available === true;

  const folder =
    step.kind === "progress"
      ? view?.folders.find((f) => f.id === step.folderId) ??
        view?.folders.find((f) => f.path !== null && f.path.toLowerCase() === step.path.toLowerCase())
      : undefined;

  const readyFolder = step.kind === "ready" ? view?.folders.find((x) => x.id === step.res.folder_id) : undefined;

  return (
    <div className="mx-auto max-w-xl space-y-6 py-6">
      <Stepper n={STEP_NO[step.kind]} />

      {(step.kind === "pick" || step.kind === "counting") && (
        <Card className="space-y-4">
          <StepTitle focus={moved}>Choose a folder to search</StepTitle>
          <CardText>
            {offered
              ? "Selene Retrieval reads a folder on this computer and keeps it up to date. Your files are not uploaded anywhere."
              : "Pick a folder with documents or code. Selene Retrieval reads it on this computer and keeps it up to date. Your files are not uploaded anywhere."}
          </CardText>
          {shownError && (
            <ErrorPanel error={shownError} onAction={onErrorAction}>
              <FolderRootsHint details={shownError.details} />
            </ErrorPanel>
          )}
          {step.kind === "counting" ? (
            <p className="flex items-center gap-2 text-sm" role="status">
              <Loader2 className="size-4 animate-spin" aria-hidden /> Counting files in “{baseName(step.path)}”…
            </p>
          ) : offered && offer ? (
            <div className="space-y-4">
              <div className="space-y-1.5">
                <Button
                  size="lg"
                  busy={maker.busy}
                  aria-describedby="create-folder-hint"
                  className="max-w-full"
                  onClick={() => void createFolder()}
                >
                  <FolderPlus /> Create a folder for me
                </Button>
                <p id="create-folder-hint" className="text-sm text-muted-foreground">
                  {defaultFolderHint(offer.name)}
                </p>
              </div>
              {maker.busy && (
                <p className="flex items-center gap-2 text-sm" role="status">
                  <Loader2 className="size-4 animate-spin" aria-hidden /> Creating your folder…
                </p>
              )}
              <Button variant="outline" className="max-w-full" disabled={maker.busy} onClick={() => void choose()}>
                <FolderSearch /> Choose an existing folder…
              </Button>
            </div>
          ) : (
            <Button size="lg" onClick={() => void choose()}>
              <FolderSearch /> Choose a folder…
            </Button>
          )}
        </Card>
      )}

      {step.kind === "big" && (
        <Card className="space-y-4">
          <StepTitle focus={moved}>This is a very big folder</StepTitle>
          <CardText>{step.est.text}</CardText>
          <div className="flex flex-wrap gap-3">
            <Button onClick={() => void choose(step.path)}>Choose a smaller folder</Button>
            <Button variant="outline" onClick={() => void add(step.path, step.est)}>
              Add it anyway
            </Button>
          </div>
        </Card>
      )}

      {(step.kind === "adding" || step.kind === "progress") && (
        <Card className="space-y-4">
          <StepTitle focus={moved && step.kind === "adding"}>Setting up search for “{baseName(step.path)}”</StepTitle>
          <CardText>{step.est.text}</CardText>
          {folder ? (
            <FolderCard folder={folder} compact />
          ) : (
            <p className="flex items-center gap-2 text-sm">
              <Loader2 className="size-4 animate-spin" aria-hidden /> Starting…
            </p>
          )}
          <CardText>
            You can keep working. Indexing continues in the background, even if you close this window.
          </CardText>
          <Button ref={nextRef} onClick={() => setStep({ kind: "provider" })} disabled={step.kind === "adding"}>
            Next
          </Button>
        </Card>
      )}

      {step.kind === "ready" && (
        <Card className="space-y-4">
          <StepTitle focus={false}>Your folder is ready</StepTitle>
          <CardText>
            Anything you put in “{step.res.name}” becomes searchable automatically. It's in your Documents folder.
            {step.res.opened && " We opened it in File Explorer for you."}
          </CardText>
          {step.res.in_onedrive && (
            <p className="flex items-start gap-1.5 text-sm text-muted-foreground">
              <Cloud className="mt-0.5 size-4 shrink-0" aria-hidden /> This folder is in OneDrive. Files kept only online
              aren't searched.
            </p>
          )}
          {readyFolder && <FolderCard folder={readyFolder} compact />}
          <CardText>
            You can keep working. Indexing continues in the background, even if you close this window.
          </CardText>
          <div className="flex flex-wrap gap-3">
            <Button ref={nextRef} onClick={() => setStep({ kind: "provider" })}>
              Next
            </Button>
            <Button variant="outline" onClick={() => openFolder(step.res.folder_id)}>
              <FolderOpen /> Open folder
            </Button>
          </div>
        </Card>
      )}

      {step.kind === "provider" && (
        <Card className="space-y-4">
          <StepTitle focus={moved}>Get written answers (optional)</StepTitle>
          <CardText>
            Search works now, on this computer. If you also want Claude and this app to write answers from your files
            with your own AI account, add it here. You can do this later in Settings.
          </CardText>
          <ProviderForm
            onErrorAction={(err) => {
              // Quota and similar: Providers & Keys opens after the wizard (page 20 state 22).
              if (err.action.kind === "open_providers") {
                setAfterFinish("/providers");
                setStep({ kind: "connect" });
                return true;
              }
              return false;
            }}
            actions={(a) => (
              <div className="flex flex-wrap gap-3">
                <Button onClick={() => setStep({ kind: "connect" })}>{a.saved ? "Next" : "Skip for now"}</Button>
                <CheckAndSaveButton a={a} />
              </div>
            )}
          />
        </Card>
      )}

      {step.kind === "connect" && (
        <Card className="space-y-5">
          <StepTitle focus={moved}>Use your folders in Claude</StepTitle>
          <ClaudeConnect />
          <label className="flex items-center gap-2 text-sm">
            <input
              type="checkbox"
              className="size-4 shrink-0 accent-[var(--color-primary)]"
              checked={autostartChecked}
              onChange={(e) => setStartWithWindows(e.target.checked)}
            />
            Start Selene Retrieval when Windows starts (recommended, keeps your folders up to date)
          </label>
          <Button size="lg" onClick={() => void finish()}>
            Finish
          </Button>
        </Card>
      )}
    </div>
  );
}
