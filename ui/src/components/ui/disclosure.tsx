import type { ReactNode } from "react";
import { Collapsible as C } from "radix-ui";
import { ChevronRight } from "lucide-react";
import { cn } from "@/lib/utils";

/**
 * DS-4.16 "Advanced" disclosure: a bordered box whose full-width trigger sits inside an `h2`
 * (so the page outline stays h1 → h2 → h3 and screen-reader users can jump to it), with the
 * DS-6 focus ring, `aria-expanded` from Radix, and a chevron that doesn't rotate under reduced
 * motion. Closed by default.
 */
export function Disclosure({
  label = "Advanced",
  defaultOpen,
  open,
  onOpenChange,
  className,
  contentClassName,
  children,
}: {
  label?: string;
  defaultOpen?: boolean;
  open?: boolean;
  onOpenChange?: (open: boolean) => void;
  className?: string;
  contentClassName?: string;
  children: ReactNode;
}) {
  return (
    <C.Root
      data-surface
      defaultOpen={defaultOpen}
      open={open}
      onOpenChange={onOpenChange}
      className={cn("rounded-lg border border-border", className)}
    >
      <h2 className="text-base font-medium">
        <C.Trigger className="group flex w-full cursor-pointer items-center gap-2 rounded-lg p-4 text-left focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-primary">
          <ChevronRight
            data-chevron
            className="size-4 transition-transform motion-reduce:transition-none group-data-[state=open]:rotate-90"
            aria-hidden
          />
          {label}
        </C.Trigger>
      </h2>
      <C.Content className={cn("px-4 pb-4", contentClassName)}>{children}</C.Content>
    </C.Root>
  );
}
