// macOS port: the few words that name the operating system or its own tools. The macOS webview
// (WKWebView) says "Macintosh" in its user agent; anything else keeps the Windows wording.

export const isMac = typeof navigator !== "undefined" && /Macintosh/.test(navigator.userAgent);

/** "Windows" / "macOS" as a name, e.g. "App version and macOS version". */
export const OS_NAME = isMac ? "macOS" : "Windows";

/** The operating system at the start of a sentence: "Your Mac didn't allow access …". */
export const OS_SUBJECT = isMac ? "Your Mac" : "Windows";

/** The file manager that "Show in …" and "We opened it in …" refer to. */
export const FILE_MANAGER = isMac ? "Finder" : "File Explorer";

/** Where API keys are kept. */
export const KEY_STORE = isMac ? "your Mac's keychain" : "Windows Credential Manager";

/** The start-at-login switch (Settings) and its wizard checkbox. */
export const START_AT_LOGIN = isMac ? "Open at login" : "Start when Windows starts";
export const START_AT_LOGIN_WIZARD = isMac
  ? "Open Selene Retrieval when you log in (recommended, keeps your folders up to date)"
  : "Start Selene Retrieval when Windows starts (recommended, keeps your folders up to date)";

export const UNINSTALL_HINT = isMac
  ? "To uninstall, quit Selene Retrieval and move it from Applications to the Trash."
  : "To uninstall, use Windows Settings → Apps.";

export const QUIT_CLAUDE_HINT = isMac
  ? "Quit Claude (Claude → Quit Claude), then open it again."
  : "Quit Claude from its icon next to the clock (or File → Exit), then open it again.";
