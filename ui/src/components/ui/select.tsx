import { Select as S } from "radix-ui";
import { Check, ChevronDown } from "lucide-react";
import { cn } from "@/lib/utils";

export type SelectOption = {
  value: string;
  label: string;
  /** Shown but not selectable (e.g. providers that need an account sign-in, not available yet). */
  disabled?: boolean;
  /** Muted text after the label, e.g. why it is disabled. */
  note?: string;
};

/**
 * DS-4.12 Select (Radix): visible label (via `aria-labelledby`), 36 px trigger, opens with the
 * keyboard, `control-off` edge.
 */
export function Select({
  id,
  labelledBy,
  value,
  onChange,
  options,
  placeholder,
  disabled,
}: {
  id?: string;
  labelledBy: string;
  value: string | null;
  onChange: (v: string) => void;
  options: SelectOption[];
  placeholder: string;
  disabled?: boolean;
}) {
  return (
    <S.Root value={value ?? undefined} onValueChange={onChange} disabled={disabled}>
      <S.Trigger
        id={id}
        aria-labelledby={labelledBy}
        className="inline-flex h-9 w-full items-center justify-between gap-2 rounded-md border border-control-off bg-background px-3 text-left text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:cursor-not-allowed disabled:opacity-50 data-[placeholder]:text-muted-foreground"
      >
        <S.Value placeholder={placeholder} />
        <S.Icon>
          <ChevronDown className="size-4 opacity-70" aria-hidden />
        </S.Icon>
      </S.Trigger>
      <S.Portal>
        <S.Content
          position="popper"
          sideOffset={4}
          data-surface
          className="z-50 max-h-80 min-w-[var(--radix-select-trigger-width)] overflow-hidden rounded-md border border-border bg-background shadow-lg"
        >
          <S.Viewport className="p-1">
            {options.map((o) => (
              <S.Item
                key={o.value}
                value={o.value}
                disabled={o.disabled}
                className={cn(
                  "relative flex cursor-pointer select-none items-center gap-2 rounded-sm py-1.5 pl-7 pr-2 text-sm outline-none",
                  "data-[highlighted]:bg-accent data-[disabled]:cursor-not-allowed data-[disabled]:opacity-60",
                )}
              >
                <S.ItemIndicator className="absolute left-2 inline-flex">
                  <Check className="size-4" aria-hidden />
                </S.ItemIndicator>
                <S.ItemText>{o.label}</S.ItemText>
                {o.note && <span className="text-xs text-muted-foreground">{o.note}</span>}
              </S.Item>
            ))}
          </S.Viewport>
        </S.Content>
      </S.Portal>
    </S.Root>
  );
}
