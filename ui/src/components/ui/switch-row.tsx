import { useId, type ReactNode } from "react";
import type { ManagedFieldView } from "@/bindings";
import { ManagedLine } from "@/components/Managed";
import { Switch } from "@/components/ui/switch";
import { cn } from "@/lib/utils";

/**
 * DS-4.11 switch row: title (the switch's accessible name, via `<label for>`), a help line (its
 * description, `aria-describedby`) and the switch. The whole title is a click target. Before M5
 * the title and help were one `<label>`, so screen readers read the help as part of the name.
 *
 * Locked rows (the switch stays focusable but `aria-disabled`; its value stays visible; the text
 * is never faded, page 33 keeps 4.5:1):
 * - `managed` (page 33, DS-4.19): the help line becomes "Managed by your organisation".
 * - `lockedBy`: the id of a section-level managed line (page 33 "per section"); the help line
 *   stays, the reason comes first in the description.
 * - `blockedHelp` (page 23 "Turn on answers first."): a switch that depends on another one.
 */
export function SwitchRow({
  label,
  help,
  checked,
  onChange,
  managed = null,
  lockedBy,
  blockedHelp,
  className,
}: {
  label: ReactNode;
  help?: ReactNode;
  checked: boolean;
  onChange: (v: boolean) => void;
  managed?: ManagedFieldView | null;
  lockedBy?: string;
  blockedHelp?: string;
  className?: string;
}) {
  const id = useId();
  const locked = managed !== null || lockedBy !== undefined || blockedHelp !== undefined;
  const helpId = `${id}-d`;
  const hasHelp = Boolean(managed || blockedHelp || help);
  const describedBy = [lockedBy, hasHelp ? helpId : undefined].filter(Boolean).join(" ") || undefined;
  return (
    <div className={cn("flex items-center justify-between gap-4", className)}>
      <div className="min-w-0 text-sm">
        <label htmlFor={`${id}-s`} className={locked ? "cursor-not-allowed" : "cursor-pointer"}>
          {label}
        </label>
        {managed ? (
          <ManagedLine field={managed} id={helpId} className="mt-0.5 flex items-start gap-1.5 text-xs text-muted-foreground" />
        ) : blockedHelp ? (
          <span id={helpId} className="block text-muted-foreground">
            {blockedHelp}
          </span>
        ) : (
          help && (
            <span id={helpId} className="block text-muted-foreground">
              {help}
            </span>
          )
        )}
      </div>
      <Switch
        id={`${id}-s`}
        checked={checked}
        aria-describedby={describedBy}
        aria-disabled={locked || undefined}
        onCheckedChange={(v) => {
          if (!locked) onChange(v);
        }}
      />
    </div>
  );
}
