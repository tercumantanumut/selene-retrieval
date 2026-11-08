import * as React from "react";
import { Slot } from "radix-ui";
import { cva, type VariantProps } from "class-variance-authority";
import { Loader2 } from "lucide-react";
import { cn } from "@/lib/utils";

// Focus ring (DS-6, OQ-11): solid 2 px primary with a 2 px background offset, so it reaches 3:1 on
// every surface. Forced-colors themes use Highlight (index.css).
const buttonVariants = cva(
  "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:pointer-events-none disabled:opacity-50 aria-disabled:opacity-50 [&_svg]:size-4 [&_svg]:shrink-0 cursor-pointer",
  {
    variants: {
      variant: {
        default: "bg-primary text-primary-foreground hover:bg-primary/90",
        outline: "border border-border bg-background hover:bg-accent",
        ghost: "hover:bg-accent",
        link: "text-primary underline-offset-4 hover:underline px-0",
        danger: "bg-danger-fill text-white hover:bg-danger-fill/90",
      },
      size: {
        default: "h-9 px-4 py-2",
        sm: "h-8 px-3",
        lg: "h-11 px-6 text-base",
      },
    },
    defaultVariants: { variant: "default", size: "default" },
  },
);

export interface ButtonProps
  extends React.ComponentProps<"button">,
    VariantProps<typeof buttonVariants> {
  asChild?: boolean;
  /** Busy (DS-4.1): disabled, with a leading spinner; the label stays the same. */
  busy?: boolean;
  /**
   * Managed by the organisation (page 33, DS-4.19): the button stays focusable and readable
   * (`aria-disabled`, outline look, `muted-foreground` text at 5.83:1, never faded) and does
   * nothing; pair it with `aria-describedby` pointing at its `ManagedLine`.
   */
  locked?: boolean;
}

export function Button({
  className,
  variant,
  size,
  asChild = false,
  busy = false,
  locked = false,
  disabled,
  children,
  onClick,
  "aria-disabled": ariaDisabled,
  ...props
}: ButtonProps) {
  const Comp = asChild ? Slot.Root : "button";
  return (
    <Comp
      className={cn(
        buttonVariants({ variant: locked ? "outline" : variant, size }),
        locked && "cursor-not-allowed text-muted-foreground hover:bg-background aria-disabled:opacity-100",
        className,
      )}
      disabled={disabled || busy}
      aria-busy={busy || undefined}
      aria-disabled={locked || ariaDisabled || undefined}
      onClick={locked ? (e: React.MouseEvent<HTMLButtonElement>) => e.preventDefault() : onClick}
      {...props}
    >
      {/* `asChild` must hand Radix Slot exactly one element: `[false, child]` makes Slot throw
          ("failed to slot onto its children", react-slot 1.5), which took down every page with
          an `asChild` button (M5-A finding, see m5-a-app.md). */}
      {asChild ? (
        children
      ) : (
        <>
          {busy && <Loader2 className="animate-spin" aria-hidden />}
          {children}
        </>
      )}
    </Comp>
  );
}
