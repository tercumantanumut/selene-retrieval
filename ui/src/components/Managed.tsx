import type { ManagedFieldView } from "@/bindings";
import { MANAGED_TEXT, PATHS, SOME_MANAGED_TEXT, usePolicy } from "@/lib/policy";
import { Lock } from "lucide-react";

/**
 * DS-4.19 managed lock: Lock icon + "Managed by your organisation" (12 px, `muted-foreground`,
 * 5.83:1) in place of the control's help line, plus the policy's plain detail ("Allowed: …")
 * when it has one. Referenced by the locked control's `aria-describedby`, so screen readers hear
 * why it can't be changed.
 */
export function ManagedLine({ field, id, className }: { field: ManagedFieldView; id?: string; className?: string }) {
  return (
    <span id={id} className={className ?? "flex items-start gap-1.5 text-xs text-muted-foreground"}>
      <Lock className="mt-px size-3.5 shrink-0" aria-hidden />
      <span>
        {field.message || MANAGED_TEXT}
        {field.detail && <span className="block">{field.detail}</span>}
      </span>
    </span>
  );
}

/**
 * A refusal by policy (`ManagedByPolicy`) of a folder outside the allowed places: the panel's own
 * text says what happened; this adds where folders *can* be (the policy's plain detail), so the
 * user has a next step (P1.5).
 */
export function FolderRootsHint({ details }: { details: string }) {
  const policy = usePolicy();
  const roots = policy.field(PATHS.folderRoots);
  if (!details.startsWith("ManagedByPolicy") || !roots?.detail) return null;
  return <p className="text-sm">{roots.detail}</p>;
}

/** Page 33: one info row under the Settings title when any setting is managed. */
export function ManagedNotice() {
  return (
    <p className="flex items-center gap-2 text-sm text-muted-foreground">
      <Lock className="size-4 shrink-0" aria-hidden />
      {SOME_MANAGED_TEXT}
    </p>
  );
}
