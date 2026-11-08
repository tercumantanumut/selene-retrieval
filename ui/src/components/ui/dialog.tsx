import * as React from "react";
import { Dialog as D } from "radix-ui";
import { cn } from "@/lib/utils";

/**
 * DS-4.9 dialog: centered, 480 px, backdrop, focus trapped and returned to the trigger on close.
 * `initialFocus` picks the least destructive control (e.g. [Don't allow]). Esc and a backdrop click
 * close it (= cancel) unless `busy` (an operation runs). `alert` renders `role="alertdialog"` (the
 * consent dialogs, page 22).
 */
export function Modal({
  open,
  onClose,
  title,
  description,
  icon,
  alert = false,
  busy = false,
  initialFocus,
  children,
  footer,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  description?: React.ReactNode;
  icon?: React.ReactNode;
  alert?: boolean;
  busy?: boolean;
  initialFocus?: React.RefObject<HTMLElement | null>;
  children?: React.ReactNode;
  footer?: React.ReactNode;
}) {
  // Most dialogs here are opened from a plain button (no `D.Trigger`), so Radix has no trigger to
  // return focus to and would leave it on <body> (WCAG 2.4.3). Remember what had focus when the
  // dialog opened (layout effect: before Radix moves focus inside) and give it back on close.
  const returnTo = React.useRef<HTMLElement | null>(null);
  React.useLayoutEffect(() => {
    if (open) {
      const el = document.activeElement;
      returnTo.current = el instanceof HTMLElement && el !== document.body ? el : null;
    }
  }, [open]);
  return (
    <D.Root
      open={open}
      onOpenChange={(o) => {
        if (!o && !busy) onClose();
      }}
    >
      <D.Portal>
        <D.Overlay className="fixed inset-0 z-40 bg-black/40" />
        <D.Content
          role={alert ? "alertdialog" : "dialog"}
          data-surface
          className={cn(
            "fixed left-1/2 top-1/2 z-50 w-[480px] max-w-[calc(100vw-48px)] -translate-x-1/2 -translate-y-1/2",
            "space-y-4 rounded-lg border border-border bg-background p-6 shadow-lg focus:outline-none",
          )}
          onOpenAutoFocus={(e) => {
            if (initialFocus?.current) {
              e.preventDefault();
              initialFocus.current.focus();
            }
          }}
          onCloseAutoFocus={(e) => {
            const el = returnTo.current;
            if (el?.isConnected) {
              e.preventDefault();
              el.focus();
            }
          }}
          onEscapeKeyDown={(e) => {
            if (busy) e.preventDefault();
          }}
          onPointerDownOutside={(e) => {
            if (busy) e.preventDefault();
          }}
          {...(description ? {} : { "aria-describedby": undefined })}
        >
          <div className="flex items-start gap-3">
            {icon}
            <D.Title className="text-lg font-semibold leading-snug">{title}</D.Title>
          </div>
          {description && (
            <D.Description asChild>
              <div className="space-y-2 text-sm">{description}</div>
            </D.Description>
          )}
          {children}
          {footer && <div className="flex flex-wrap justify-end gap-2 pt-1">{footer}</div>}
        </D.Content>
      </D.Portal>
    </D.Root>
  );
}
