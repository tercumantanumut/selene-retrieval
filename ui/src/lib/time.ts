// Plain time words for "Last checked 5 minutes ago", "Last used yesterday", Activity day headings.
// Computed at render (each push or user action re-renders); nothing ticks on a timer (D-16).

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

/** "18 October 2026, 10:37" for `title` tooltips (WCAG: relative times get an absolute title). */
export function absoluteTime(ms: number): string {
  const d = new Date(ms);
  const date = d.toLocaleDateString("en-GB", { day: "numeric", month: "long", year: "numeric" });
  return `${date}, ${clock(ms)}`;
}

/** "10:21" (24-hour, as in the mockups). */
export function clock(ms: number): string {
  return new Date(ms).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
}

function startOfDay(ms: number): number {
  const d = new Date(ms);
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

/** "just now", "5 minutes ago", "2 hours ago", "yesterday", "3 days ago", "on 2 October". */
export function relativeTime(ms: number, now = Date.now()): string {
  const diff = Math.max(0, now - ms);
  if (diff < MINUTE) return "just now";
  if (diff < HOUR) {
    const m = Math.floor(diff / MINUTE);
    return `${m} minute${m === 1 ? "" : "s"} ago`;
  }
  const days = Math.round((startOfDay(now) - startOfDay(ms)) / DAY);
  if (days === 0) {
    const h = Math.floor(diff / HOUR);
    return `${h} hour${h === 1 ? "" : "s"} ago`;
  }
  if (days === 1) return "yesterday";
  if (days < 7) return `${days} days ago`;
  return `on ${new Date(ms).toLocaleDateString("en-GB", { day: "numeric", month: "long" })}`;
}

/** Activity day heading: "Today", "Yesterday", or "Friday, 16 October". */
export function dayHeading(ms: number, now = Date.now()): string {
  const days = Math.round((startOfDay(now) - startOfDay(ms)) / DAY);
  if (days === 0) return "Today";
  if (days === 1) return "Yesterday";
  return new Date(ms).toLocaleDateString("en-GB", { weekday: "long", day: "numeric", month: "long" });
}

/** "Today 10:30", "Yesterday 16:20", "16 October 09:02" (egress table). */
export function dayAndTime(ms: number, now = Date.now()): string {
  const h = dayHeading(ms, now);
  if (h === "Today" || h === "Yesterday") return `${h} ${clock(ms)}`;
  return `${new Date(ms).toLocaleDateString("en-GB", { day: "numeric", month: "long" })} ${clock(ms)}`;
}
