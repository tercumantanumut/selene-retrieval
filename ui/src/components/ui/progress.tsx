import { cn } from "@/lib/utils";

/**
 * Determinate when `value` is a number (0–100), indeterminate otherwise (DS-4.5). Every bar has an
 * accessible name and value text, so screen readers hear "Indexing 340 of 2,100 files" rather
 * than a bare percentage. Reduced motion: no width animation, the indeterminate segment is static.
 */
export function Progress({
  value,
  className,
  label,
  valueText,
}: {
  value: number | null;
  className?: string;
  label?: string;
  valueText?: string;
}) {
  return (
    <div
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={value ?? undefined}
      aria-valuetext={valueText}
      data-progress-track
      className={cn("relative h-2 w-full overflow-hidden rounded-full bg-muted", className)}
    >
      {value === null ? (
        <div data-progress-fill className="absolute inset-y-0 left-0 w-1/3 animate-pulse rounded-full bg-primary/60" />
      ) : (
        <div
          data-progress-fill
          className="h-full rounded-full bg-primary transition-[width] duration-300 motion-reduce:transition-none"
          style={{ width: `${Math.max(2, Math.min(100, value))}%` }}
        />
      )}
    </div>
  );
}
