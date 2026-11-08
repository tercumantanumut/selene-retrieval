// The one error model (sr-common `ErrorCode`, #1–#41) on the UI side. The Rust shell normally
// hands the UI a finished `UiError` (plain title + body + one action, rendered by sr-common), so the
// UI never writes its own wording. Two jobs remain here:
//  1. `errorCode()` reads the stable code back from `UiError.details` ("<Code> · trace <id>") so a
//     page can branch on it (e.g. no [Retry] for codes whose retry policy is "No").
//  2. `fromWire()` renders a raw RPC error (`{ code, ctx }`) that reached the UI without passing
//     through sr-common (defensive; the shell maps these already). Text is copied verbatim from
//     `crates/common/src/error/messages.rs`; anything else becomes "Something went wrong".
import type { UiError } from "@/bindings";

/** Mirrors `sr_common::error::ErrorCode::ALL`, in order (#1–#41). */
export const ERROR_CODES = [
  "InvalidInput",
  "NotFound",
  "PathOutsideSandbox",
  "PathRefused",
  "PathDenied",
  "FolderOverlap",
  "FolderUnavailable",
  "PermissionDenied",
  "FileTooLarge",
  "FileLocked",
  "FileOnlineOnly",
  "FileUnreadable",
  "ExtractTimeout",
  "ApprovalRequired",
  "ApprovalDenied",
  "Conflict",
  "Busy",
  "Cancelled",
  "Timeout",
  "ProviderNotConfigured",
  "ProviderAuth",
  "ProviderQuota",
  "RateLimited",
  "ProviderUnavailable",
  "ProviderRequest",
  "PrivacyBlocked",
  "Offline",
  "NetworkBlocked",
  "ModelDownloadFailed",
  "ModelIntegrity",
  "DiskFull",
  "Corrupt",
  "ServiceUnavailable",
  "UpgradeRequired",
  "SettingsInvalid",
  "Internal",
  "ModelIncompatible",
  "IndexMissing",
  "ManagedByPolicy",
  "ConnectionNotTrusted",
  "PortInUse",
] as const;

export type ErrorCode = (typeof ERROR_CODES)[number];

const KNOWN = new Set<string>(ERROR_CODES);

/** Codes where trying again can't help (sr-common retry policy "No" that the UI acts on). */
const NO_RETRY: ReadonlySet<ErrorCode> = new Set<ErrorCode>(["ConnectionNotTrusted", "ManagedByPolicy"]);

/** The code a `UiError` was made from, or null for UI-made errors. */
export function errorCode(e: Pick<UiError, "details">): ErrorCode | null {
  const head = e.details.split(" · ")[0]?.trim() ?? "";
  return KNOWN.has(head) ? (head as ErrorCode) : null;
}

/** Whether a [Retry] fallback makes sense for this error. */
export function canRetry(e: Pick<UiError, "details">): boolean {
  const c = errorCode(e);
  return c === null || !NO_RETRY.has(c);
}

/** sr-common's plain text for codes the UI may meet raw. Verbatim, never paraphrased. */
const PLAIN: Partial<Record<ErrorCode, Pick<UiError, "title" | "body">>> = {
  // #40 (M5-E, S-01): a pipe squatter. Nothing was sent; only a restart frees the name.
  ConnectionNotTrusted: {
    title: "Connection not safe",
    body: "Another program is using Selene Retrieval's connection, so nothing was sent. Restart Windows to fix this.",
  },
};

/** A raw `{ code, ctx }` RPC error → UiError, or null when it isn't one. */
export function fromWire(e: unknown): UiError | null {
  if (typeof e !== "object" || e === null || !("code" in e)) return null;
  const code = String((e as { code: unknown }).code);
  if (!KNOWN.has(code)) return null;
  const text = PLAIN[code as ErrorCode];
  if (!text) return null;
  // ConnectionNotTrusted has no button (sr-common `UiAction::None`): the only fix is a restart.
  return { ...text, action: { kind: "none" }, action_label: "", details: code };
}
