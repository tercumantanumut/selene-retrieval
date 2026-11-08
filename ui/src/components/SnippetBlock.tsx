import { Button } from "@/components/ui/button";
import { copyText } from "@/lib/app";
import { ClipboardCopy } from "lucide-react";

/**
 * DS-4.13: read-only setup text (monospace 12 px, scrolls sideways, max 12 lines) with [Copy]
 * above its top right. `shown` is what is on screen (a token masked as `••••` + last 4, OQ-45);
 * `copy` is what [Copy] copies. The block is focusable, so Ctrl+C copies a selection too.
 */
export function SnippetBlock({
  shown,
  copy,
  copiedText = "Setup text copied. Paste it into your AI app's MCP settings, then restart that app.",
}: {
  shown: string;
  copy?: string;
  copiedText?: string;
}) {
  return (
    <div className="space-y-2">
      <div className="flex justify-end">
        <Button size="sm" variant="outline" onClick={() => void copyText(copy ?? shown, copiedText)}>
          <ClipboardCopy /> Copy
        </Button>
      </div>
      <pre
        tabIndex={0}
        aria-label="Setup text"
        data-surface
        className="max-h-[calc(12*1.5em+1.5rem)] overflow-auto rounded-lg bg-muted p-3 font-mono text-xs leading-[1.5em] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-primary focus-visible:ring-offset-2 focus-visible:ring-offset-background"
      >
        {shown}
      </pre>
    </div>
  );
}
