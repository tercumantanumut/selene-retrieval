import { open } from "@tauri-apps/plugin-dialog";
import type { FolderView } from "@/bindings";

/** Windows folder picker. `null` when the user cancels. */
export async function pickFolder(defaultPath?: string): Promise<string | null> {
  const dir = await open({ directory: true, multiple: false, title: "Choose a folder", defaultPath });
  return typeof dir === "string" ? dir : null;
}

/** Last path segment, for plain sentences ("Adding “Documents”…"). */
export function baseName(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

/**
 * Page 08 / 24 sort: problems and folders waiting for the user's choice (Amendment 17) first, then
 * busy, then the rest by name.
 */
export function sortFolders(list: FolderView[]): FolderView[] {
  const rank = (f: FolderView) =>
    f.state === "unavailable" || f.state === "problem" || f.was_files_only
      ? 0
      : f.state === "waiting" || f.state === "scanning" || f.state === "indexing"
        ? 1
        : 2;
  return [...list].sort((a, b) => rank(a) - rank(b) || a.name.localeCompare(b.name));
}
