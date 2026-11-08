import { useEffect, useRef, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { toast } from "sonner";
import { commands } from "@/bindings";
import { Button } from "@/components/ui/button";
import { CLAUDE_KEY, CLIENTS_KEY, copySnippet, unwrap, useClaudeStatus, useErrorToast } from "@/lib/app";
import { CheckCircle2, ClipboardCopy, Plug } from "lucide-react";

/**
 * OQ-46 / R-UX28 [Repair…]: behind a plain inline confirm; the broken file is kept next to it.
 * Shared by every app card on Connected apps.
 */
export function RepairConfirm({ app, short, onDone }: { app: "claude_desktop" | "cursor" | "vs_code"; short: string; onDone: () => void }) {
  const qc = useQueryClient();
  const showError = useErrorToast();
  const [busy, setBusy] = useState(false);
  const cancelRef = useRef<HTMLButtonElement>(null);
  useEffect(() => cancelRef.current?.focus(), []);
  const repair = async () => {
    setBusy(true);
    try {
      const msg = await unwrap(commands.repairClient(app));
      toast.success(msg);
      void qc.invalidateQueries({ queryKey: CLIENTS_KEY });
      void qc.invalidateQueries({ queryKey: CLAUDE_KEY });
      onDone();
    } catch (e) {
      showError(e, () => void repair());
    } finally {
      setBusy(false);
    }
  };
  return (
    <div
      role="group"
      aria-label={`Repair ${short}'s settings?`}
      className="flex flex-wrap items-center gap-2"
      onKeyDown={(e) => {
        if (e.key === "Escape") onDone();
      }}
    >
      <p className="text-sm" aria-live="polite">
        Back up the broken file and write a fresh one that contains only Selene Retrieval? {short}'s other settings in
        that file will be lost.
      </p>
      <Button size="sm" variant="danger" busy={busy} onClick={() => void repair()}>
        Repair
      </Button>
      <Button ref={cancelRef} size="sm" variant="ghost" disabled={busy} onClick={onDone}>
        Cancel
      </Button>
    </div>
  );
}

/**
 * "Connect Claude Desktop" button, or the copy-paste setup text (P1.4). `manage` (Connected apps,
 * page 29) adds [Disconnect] when connected and [Repair…] when the file can't be read.
 */
export function ClaudeConnect({ manage = false }: { manage?: boolean }) {
  const status = useClaudeStatus();
  const qc = useQueryClient();
  const showError = useErrorToast();
  const [message, setMessage] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [repairing, setRepairing] = useState(false);
  if (!status) return null;

  const connect = async () => {
    setBusy(true);
    try {
      const r = await unwrap(commands.connectClaude());
      qc.setQueryData(CLAUDE_KEY, r.status);
      void qc.invalidateQueries({ queryKey: CLIENTS_KEY });
      setMessage(r.message);
    } catch (e) {
      showError(e, () => void connect());
    } finally {
      setBusy(false);
    }
  };
  const disconnect = async () => {
    setBusy(true);
    try {
      const r = await unwrap(commands.disconnectClient("claude_desktop"));
      void qc.invalidateQueries({ queryKey: CLAUDE_KEY });
      void qc.invalidateQueries({ queryKey: CLIENTS_KEY });
      setMessage(null);
      toast.success(r);
    } catch (e) {
      showError(e, () => void disconnect());
    } finally {
      setBusy(false);
    }
  };
  const copy = () => void copySnippet(status.snippet);

  switch (status.claude) {
    case "connected":
      return (
        <div className="space-y-2">
          <p className="flex items-center gap-2 font-medium">
            <CheckCircle2 className="size-5 text-success" aria-hidden /> Claude Desktop is connected
          </p>
          <p className="text-sm text-muted-foreground">
            {message ??
              "Ask Claude about your files, for example: “Search my documents for the lease agreement.”"}
          </p>
          {manage && (
            <Button size="sm" variant="ghost" disabled={busy} onClick={() => void disconnect()}>
              Disconnect
            </Button>
          )}
        </div>
      );
    case "not_installed":
      return (
        <div className="space-y-3">
          <p className="text-sm text-muted-foreground">
            Claude Desktop isn't installed on this computer. After you install it, connect it here. For other AI
            apps, copy the setup text and paste it into that app's MCP settings.
          </p>
          <Button variant="outline" onClick={copy}>
            <ClipboardCopy /> Copy setup text
          </Button>
        </div>
      );
    case "cant_read":
      return (
        <div className="space-y-3">
          <p className="text-sm text-muted-foreground">
            Claude's settings file couldn't be read, so we didn't change it. Copy the setup text and paste it into
            Claude's settings instead (Settings → Developer → Edit Config).
          </p>
          {repairing ? (
            <RepairConfirm app="claude_desktop" short="Claude" onDone={() => setRepairing(false)} />
          ) : (
            <div className="flex flex-wrap gap-2">
              <Button variant="outline" onClick={copy}>
                <ClipboardCopy /> Copy setup text
              </Button>
              {manage && (
                <Button variant="ghost" onClick={() => setRepairing(true)}>
                  Repair…
                </Button>
              )}
            </div>
          )}
        </div>
      );
    case "not_connected":
    case "needs_reconnect": {
      const reconnect = status.claude === "needs_reconnect" || status.changed_externally;
      return (
        <div className="space-y-3">
          <p className="text-sm text-muted-foreground">
            {status.claude === "needs_reconnect"
              ? "Claude's connection points to an old copy of Selene Retrieval. Reconnect to fix it."
              : status.changed_externally
                ? "Claude's settings were changed and no longer include Selene Retrieval."
                : "One click lets Claude search the folders you added. Nothing else in Claude's settings changes."}
          </p>
          <div className="flex flex-wrap items-center gap-3">
            <Button onClick={() => void connect()} disabled={busy}>
              <Plug /> {reconnect ? "Reconnect Claude Desktop" : "Connect Claude Desktop"}
            </Button>
            <Button variant="link" onClick={copy}>
              Copy setup text instead
            </Button>
          </div>
        </div>
      );
    }
  }
}
