; Selene Retrieval installer hooks (owner: M2-I; M5-I: notice + wording).
;
; Included by BOTH installers, so the install/uninstall behaviour is identical:
;   * the Tauri NSIS bundler (tauri.conf.json: bundle.windows.nsis.installerHooks = "nsis/hooks.nsh")
;   * installer/nsis/selene-retrieval.nsi, the standalone script (`cargo xtask package`), used
;     until the Tauri bundler takes over (no tauri-cli in the build yet).
; Both provide what these macros use: $INSTDIR, $PLUGINSDIR, $UpdateMode (1 = updater / reinstall
; pass), $DeleteAppDataCheckboxState (uninstall-page checkbox, 1 = also delete the search index),
; LogicLib and FileFunc. Nothing here needs admin rights (per-user install, no UAC prompt).
; The MSI (installer/wix/) runs the same sr-setup commands from its custom actions.
;
; All real work is done by sr-setup.exe (installer/sr-setup), so task names, task XML, the
; install-folder ACE and the sandbox profile name come from the same code the daemon uses:
;   install   : inheritable ALL APPLICATION PACKAGES RX ACE on $INSTDIR (Amendment 4.5),
;               data folder, per-user on-demand task \SR\daemon-<sid8> (Amendment 4.6)
;   stop      : drain the background service (host.shutdown), stop leftovers; on update, move
;               in-use sr-bridge.exe aside instead of killing MCP clients' bridges
;   uninstall : delete the task (and the \SR task folder once empty), our entry in the
;               Claude Desktop, Cursor and VS Code configs (the app's own Connect writers;
;               other servers untouched; our .selene-retrieval.bak backups removed),
;               the extractor's AppContainer profile (Amendment 3.17), every saved AI provider
;               key and HTTP tool password in Credential Manager (M3/M4; generic credentials
;               named "SeleneRetrieval/*",
;               installer/sr-setup/src/creds.rs), the autostart values and, only if the
;               checkbox is ticked (or /DELETEAPPDATA on a silent uninstall), the data folder
;               %LOCALAPPDATA%\SeleneRetrieval (index, catalog, settings, logs)
; Uninstall and update never run the helper from $INSTDIR: a copy in $PLUGINSDIR is used so no
; installed file stays open. Every step is idempotent; a failed step is logged, never fatal,
; because the daemon self-heals the ACE and sr-launch re-registers a missing task.
;
; R-UX43 (M5-I): a client settings file we may not edit (a VS Code mcp.json with comments, or a
; file that couldn't be written) that still lists us is named in a summary box at the end of the
; uninstall, with the one thing to do. sr-setup writes the text (--notice-file).

!include LogicLib.nsh
!include FileFunc.nsh

!define SR_NOTE_INSTALL "Some setup steps will finish the first time Selene Retrieval starts."
!define SR_NOTE_STOP "The background search service had to be stopped; nothing was lost."
!define SR_NOTE_UNINSTALL "Some items could not be removed; they are listed above."
!define SR_NOTICE_FILE "$PLUGINSDIR\sr-uninstall-notice.txt"

; Runs the staged helper with ARGS and logs its plain-language lines into the details view. NOTE
; is the details line when a step reported a problem (the old line said "it completes on first
; start" for every step, also on uninstall where nothing starts again; R-UX43).
!macro SR_SETUP ARGS NOTE
  Push $0
  nsExec::ExecToLog `"$PLUGINSDIR\sr-setup.exe" ${ARGS}`
  Pop $0
  ${If} $0 != 0
    DetailPrint "${NOTE} (code $0)"
  ${EndIf}
  Pop $0
!macroend

; Copies the installed helper into $PLUGINSDIR (deleted when the installer exits).
!macro SR_STAGE_HELPER
  InitPluginsDir
  Delete "$PLUGINSDIR\sr-setup.exe"
  ${If} ${FileExists} "$INSTDIR\sr-setup.exe"
    CopyFiles /SILENT "$INSTDIR\sr-setup.exe" "$PLUGINSDIR\sr-setup.exe"
  ${EndIf}
!macroend

; Shows sr-setup's uninstall summary once (UTF-16LE text it wrote only when there is something
; the user must finish by hand). A silent uninstall skips the box (/SD) and keeps the log line.
!macro SR_SHOW_NOTICE
  ${If} ${FileExists} "${SR_NOTICE_FILE}"
    Push $0
    Push $1
    Push $2
    StrCpy $2 ""
    ClearErrors
    FileOpen $0 "${SR_NOTICE_FILE}" r
    ${IfNot} ${Errors}
      ${Do}
        ClearErrors
        FileReadUTF16LE $0 $1
        ${If} ${Errors}
          ${ExitDo}
        ${EndIf}
        StrCpy $2 "$2$1"
      ${Loop}
      FileClose $0
    ${EndIf}
    Delete "${SR_NOTICE_FILE}"
    ${If} $2 != ""
      DetailPrint "$2"
      MessageBox MB_OK|MB_ICONINFORMATION "$2" /SD IDOK
    ${EndIf}
    Pop $2
    Pop $1
    Pop $0
  ${EndIf}
!macroend

; Before files are written. On an update or reinstall the *old* helper drains the *old* daemon.
!macro NSIS_HOOK_PREINSTALL
  ; M5-I: the MSI (organisation package) owns the same folder; two installers must never manage
  ; one install. Its registry value goes away when the MSI is removed.
  ReadRegStr $R8 HKCU "Software\Selene\Selene Retrieval" "InstalledBy"
  ${If} $R8 == "msi"
    MessageBox MB_OK|MB_ICONINFORMATION "Selene Retrieval is already installed from its MSI package (usually set up by your organisation). Update it the same way. Nothing was changed." /SD IDOK
    SetErrorLevel 1638
    Quit
  ${EndIf}
  !insertmacro SR_STAGE_HELPER
  ${If} ${FileExists} "$PLUGINSDIR\sr-setup.exe"
    DetailPrint "Stopping the background search service for the update..."
    !insertmacro SR_SETUP 'stop --install-dir "$INSTDIR" --mode update' "${SR_NOTE_STOP}"
  ${EndIf}
!macroend

; After files are written: folder access for the sandboxed reader, data folder, scheduled task.
!macro NSIS_HOOK_POSTINSTALL
  DetailPrint "Setting up the background search service..."
  !insertmacro SR_STAGE_HELPER
  !insertmacro SR_SETUP 'install --install-dir "$INSTDIR"' "${SR_NOTE_INSTALL}"
!macroend

; Before files are removed.
!macro NSIS_HOOK_PREUNINSTALL
  ; A silent uninstall has no checkbox: /DELETEAPPDATA opts in to deleting the index.
  ClearErrors
  ${GetOptions} $CMDLINE "/DELETEAPPDATA" $R9
  ${IfNot} ${Errors}
    StrCpy $DeleteAppDataCheckboxState 1
  ${EndIf}
  !insertmacro SR_STAGE_HELPER
  ${If} ${FileExists} "$PLUGINSDIR\sr-setup.exe"
    ${If} $UpdateMode = 1
      DetailPrint "Stopping the background search service for the update..."
      !insertmacro SR_SETUP 'stop --install-dir "$INSTDIR" --mode update' "${SR_NOTE_STOP}"
    ${Else}
      DetailPrint "Stopping the background search service..."
      !insertmacro SR_SETUP 'stop --install-dir "$INSTDIR" --mode uninstall' "${SR_NOTE_STOP}"
      ${If} $DeleteAppDataCheckboxState = 1
        DetailPrint "Removing the background service and your search index..."
        !insertmacro SR_SETUP 'uninstall --install-dir "$INSTDIR" --delete-data --notice-file "${SR_NOTICE_FILE}"' "${SR_NOTE_UNINSTALL}"
      ${Else}
        DetailPrint "Removing the background service (your search index is kept)..."
        !insertmacro SR_SETUP 'uninstall --install-dir "$INSTDIR" --notice-file "${SR_NOTICE_FILE}"' "${SR_NOTE_UNINSTALL}"
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend

; After files are removed: leftovers of an earlier update's move-aside, then the empty folders.
; Never `RMDir /r $INSTDIR`: the user may have picked a folder that holds other files.
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    Delete "$INSTDIR\*.sr-old"
    RMDir "$INSTDIR\models\bge-small-en-v1.5"
    RMDir "$INSTDIR\models"
    RMDir "$INSTDIR\licenses"
    RMDir "$INSTDIR"
    ; The app's own WebView2 cache and UI state (not the index, which lives in
    ; %LOCALAPPDATA%\SeleneRetrieval and follows the checkbox). Tauri removes these only when the
    ; checkbox is ticked; uninstall must leave nothing else behind, so they always go.
    ; Autostart: Tauri removes the Run value; the plugin also leaves StartupApproved (R-UX25).
    ; (sr-setup uninstall deletes both values too; this stays for a missing helper.)
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Selene Retrieval"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "Selene Retrieval"
    !ifdef BUNDLEID
      !if "${BUNDLEID}" != ""
        SetShellVarContext current
        RMDir /r "$LOCALAPPDATA\${BUNDLEID}"
        RMDir /r "$APPDATA\${BUNDLEID}"
      !endif
    !endif
    !insertmacro SR_SHOW_NOTICE
  ${EndIf}
!macroend
