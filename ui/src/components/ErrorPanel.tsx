import type { ReactNode } from "react";
import type { UiError } from "@/bindings";
import { Button } from "@/components/ui/button";
import { useRunAction } from "@/lib/app";
import { AlertTriangle } from "lucide-react";

/**
 * Inline error (DS-4.7): plain title + body, one action, technical details folded away. The icon
 * uses `warning-strong` (5.56:1) and the body full `foreground` (OQ-11). `live="status"` for panels
 * already present when a page loads (notices), so navigating doesn't re-announce them; `alert`
 * (default) for panels that appear in response to a change. `children` adds one extra line under
 * the actions (e.g. "Save without checking" when offline).
 */
export function ErrorPanel({
  error,
  onRetry,
  onAction,
  onDismiss,
  live = "alert",
  children,
}: {
  error: UiError;
  onRetry?: () => void;
  onAction?: () => void;
  /** Notices can be hidden until the next start. */
  onDismiss?: () => void;
  live?: "alert" | "status";
  children?: ReactNode;
}) {
  const run = useRunAction();
  return (
    <div role={live} data-surface className="rounded-lg border border-warning/40 bg-warning/10 p-4">
      <div className="flex items-start gap-3">
        <AlertTriangle className="mt-0.5 size-5 shrink-0 text-warning-strong" aria-hidden />
        <div className="min-w-0 flex-1 space-y-2">
          <p className="font-medium">{error.title}</p>
          <p className="text-sm">{error.body}</p>
          {((error.action.kind !== "none" && error.action_label) || onDismiss) && (
            <div className="flex flex-wrap gap-2">
              {error.action.kind !== "none" && error.action_label && (
                <Button size="sm" variant="outline" onClick={() => (onAction ? onAction() : run(error, onRetry))}>
                  {error.action_label}
                </Button>
              )}
              {onDismiss && (
                <Button size="sm" variant="ghost" onClick={onDismiss}>
                  Dismiss
                </Button>
              )}
            </div>
          )}
          {children}
          <details className="text-xs text-muted-foreground">
            <summary className="cursor-pointer select-none">Details</summary>
            <code className="mt-1 block break-all">{error.details}</code>
          </details>
        </div>
      </div>
    </div>
  );
}
