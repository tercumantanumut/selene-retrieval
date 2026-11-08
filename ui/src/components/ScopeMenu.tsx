import type { FolderView } from "@/bindings";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuCheckboxItem,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { ChevronDown } from "lucide-react";

/**
 * OQ-41 scope menu: "All folders ▾" with a checkbox per folder. `null` = all folders (default).
 * The choice lives in the page only and is not remembered between sessions.
 */
export function ScopeMenu({
  folders,
  value,
  onChange,
  label,
}: {
  folders: FolderView[];
  value: string[] | null;
  onChange: (v: string[] | null) => void;
  /** Accessible name prefix, e.g. "Search in" or "Use context from". */
  label: string;
}) {
  const usable = folders.filter((f) => f.state !== "removing");
  const chosen = value ?? usable.map((f) => f.id);
  const text =
    value === null || chosen.length === usable.length
      ? "All folders"
      : chosen.length === 1
        ? (usable.find((f) => f.id === chosen[0])?.name ?? "1 folder")
        : `${chosen.length} folders`;
  const toggle = (id: string, on: boolean) => {
    const next = on ? [...new Set([...chosen, id])] : chosen.filter((x) => x !== id);
    if (next.length === 0) return; // at least one folder stays chosen
    onChange(next.length === usable.length ? null : next);
  };
  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>
        <Button type="button" variant="outline" aria-label={`${label}: ${text}`} className="max-w-[16rem]">
          <span className="truncate">{text}</span>
          <ChevronDown aria-hidden />
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="max-h-80 overflow-y-auto">
        <DropdownMenuItem onSelect={() => onChange(null)}>All folders</DropdownMenuItem>
        <DropdownMenuSeparator />
        {usable.map((f) => (
          <DropdownMenuCheckboxItem
            key={f.id}
            checked={chosen.includes(f.id)}
            onCheckedChange={(on) => toggle(f.id, on === true)}
            onSelect={(e) => e.preventDefault()}
          >
            <span className="truncate" title={f.path ?? f.name}>
              {f.name}
            </span>
          </DropdownMenuCheckboxItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}
