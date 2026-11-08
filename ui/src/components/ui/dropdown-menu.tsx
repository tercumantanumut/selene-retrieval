import * as React from "react";
import { DropdownMenu as M } from "radix-ui";
import { cn } from "@/lib/utils";

// DS-4.17 "More" / "Menu" and the OQ-41 scope menu (Radix DropdownMenu: arrow keys, Esc closes,
// focus returns to the trigger).
export const DropdownMenu = M.Root;
export const DropdownMenuTrigger = M.Trigger;

export function DropdownMenuContent({ className, ...props }: React.ComponentProps<typeof M.Content>) {
  return (
    <M.Portal>
      <M.Content
        sideOffset={6}
        align="start"
        data-surface
        className={cn(
          "z-50 min-w-[220px] rounded-lg border border-border bg-background p-1 shadow-lg focus:outline-none",
          className,
        )}
        {...props}
      />
    </M.Portal>
  );
}

const itemClass =
  "flex cursor-pointer select-none items-center gap-2.5 rounded-md px-2.5 py-2 text-sm outline-none data-[highlighted]:bg-accent data-[disabled]:cursor-not-allowed data-[disabled]:opacity-60 [&_svg]:size-4 [&_svg]:shrink-0";

export function DropdownMenuItem({ className, ...props }: React.ComponentProps<typeof M.Item>) {
  return <M.Item className={cn(itemClass, className)} {...props} />;
}

export function DropdownMenuCheckboxItem({
  className,
  children,
  ...props
}: React.ComponentProps<typeof M.CheckboxItem>) {
  return (
    <M.CheckboxItem className={cn(itemClass, className)} {...props}>
      <span
        aria-hidden
        className="inline-flex size-4 items-center justify-center rounded-sm border border-control-off data-[state=checked]:bg-primary"
        data-state={props.checked ? "checked" : "unchecked"}
      >
        <M.ItemIndicator>
          <svg viewBox="0 0 24 24" className="size-3 fill-none stroke-primary-foreground stroke-[3]">
            <path d="M20 6 9 17l-5-5" />
          </svg>
        </M.ItemIndicator>
      </span>
      {children}
    </M.CheckboxItem>
  );
}

export function DropdownMenuSeparator() {
  return <M.Separator className="my-1 h-px bg-border" />;
}

export function DropdownMenuLabel({ className, ...props }: React.ComponentProps<typeof M.Label>) {
  return <M.Label className={cn("px-2.5 py-1.5 text-xs text-muted-foreground", className)} {...props} />;
}
