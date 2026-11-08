import type { ReactNode } from "react";
import type { LucideIcon } from "lucide-react";

/** DS-4.10: a 32 px muted icon, a title, one sentence and at most one primary button. */
export function EmptyState({
  icon: Icon,
  title,
  text,
  action,
  headingLevel = 2,
}: {
  icon: LucideIcon;
  title: string;
  text?: string;
  action?: ReactNode;
  headingLevel?: 1 | 2;
}) {
  const H = headingLevel === 1 ? "h1" : "h2";
  return (
    <div className="flex flex-col items-center gap-2 px-6 py-10 text-center">
      <Icon className="size-8 text-muted-foreground" strokeWidth={1.5} aria-hidden />
      <H className="text-base font-semibold">{title}</H>
      {text && <p className="text-sm text-muted-foreground">{text}</p>}
      {action && <div className="pt-1">{action}</div>}
    </div>
  );
}
