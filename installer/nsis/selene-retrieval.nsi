; Selene Retrieval: standalone per-user NSIS installer (owner: M2-I).
;
; Used until the Tauri bundler takes over (packages the real Tauri sr-app, or the M0 stub). It mirrors the Tauri NSIS template where it
; matters: the same hook macros (crates/app/nsis/hooks.nsh) at the same points, the same variables
; ($UpdateMode, $PassiveMode, $DeleteAppDataCheckboxState), per-user install under %LOCALAPPDATA%,
; HKCU uninstall entry, Start menu shortcut. When the Tauri bundler takes over, this file retires
; and hooks.nsh stays.
;
; Built only by `cargo xtask package`, which passes:
;   SR_VERSION  x.y.z           SR_STAGE  staged files dir     SR_FILES  generated files.nsh
;   SR_OUTFILE  installer path  SR_HOOKS  hooks.nsh            SR_SIGN_CMD (optional) signer
;   SR_STUB_APP (sr-app is the console stub or has no embedded UI: no "Open" on the finish page)
;   BUNDLEID    tauri.conf.json identifier (its app-data folders are removed on uninstall)
;   SR_WEBVIEW2 Microsoft's WebView2 Evergreen bootstrapper (Authenticode-checked by xtask); run
;               only when WebView2 is missing, so Windows 10 machines without it get the app window
;   SR_WEBVIEW2_OFFLINE Microsoft's WebView2 Evergreen *Standalone* x64 runtime installer
;               (Authenticode-checked by xtask): installs WebView2 with no internet connection.
;               Only the separate offline variant sets it (`xtask package --webview2-offline`,
;               SeleneRetrieval-<v>-x64-offline-setup.exe); the default installer bundles the
;               small bootstrapper (owner default, Amendment 7). If both were defined the
;               standalone installer would be used.
;
; Command line: /S silent, /P passive, /UPDATE (updater pass), /D=<dir> install folder (last).
; Uninstaller: /S silent, /DELETEAPPDATA also delete the search index (default: keep it).

Unicode true
; Setup and uninstaller manifests (M5-I, same rules as installer/manifests/; verified on the setup
; exe by `cargo xtask package` through check-manifest): asInvoker (RequestExecutionLevel user),
; supportedOS incl. Windows 10/11, long paths, per-monitor DPI (sharp text on high-DPI screens).
ManifestDPIAware true
ManifestDPIAwareness "PerMonitorV2,System"
ManifestLongPathAware true
ManifestSupportedOS all
RequestExecutionLevel user
SetCompressor /SOLID lzma
SetCompressorDictSize 64

!macro SR_REQUIRE NAME
  !ifndef ${NAME}
    !error "${NAME} is not defined: build with `cargo xtask package`"
  !endif
!macroend
!insertmacro SR_REQUIRE SR_VERSION
!insertmacro SR_REQUIRE SR_STAGE
!insertmacro SR_REQUIRE SR_FILES
!insertmacro SR_REQUIRE SR_OUTFILE
!insertmacro SR_REQUIRE SR_HOOKS

!define PRODUCTNAME "Selene Retrieval"
!ifndef BUNDLEID
  !define BUNDLEID ""   ; tauri.conf.json identifier, passed by `cargo xtask package`
!endif
!define MANUFACTURER "Selene"
!define MAINBINARYNAME "sr-app"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\SeleneRetrieval"
!define RUNKEY "Software\Microsoft\Windows\CurrentVersion\Run"

Name "${PRODUCTNAME}"
OutFile "${SR_OUTFILE}"
InstallDir "$LOCALAPPDATA\${PRODUCTNAME}"
InstallDirRegKey HKCU "${UNINSTKEY}" "InstallLocation"
BrandingText "${PRODUCTNAME} ${SR_VERSION}"
ShowInstDetails nevershow
ShowUninstDetails nevershow

VIProductVersion "${SR_VERSION}.0"
VIAddVersionKey "ProductName" "${PRODUCTNAME}"
VIAddVersionKey "FileDescription" "${PRODUCTNAME} Setup"
VIAddVersionKey "CompanyName" "${MANUFACTURER}"
VIAddVersionKey "FileVersion" "${SR_VERSION}"
VIAddVersionKey "ProductVersion" "${SR_VERSION}"
VIAddVersionKey "LegalCopyright" "${MANUFACTURER}"

!ifdef SR_SIGN_CMD
  ; Sign the uninstaller before it is embedded, then the finished installer (NSIS >= 3.08).
  !uninstfinalize '${SR_SIGN_CMD} "%1"' = 0
  !finalize '${SR_SIGN_CMD} "%1"' = 0
!endif

!include MUI2.nsh
!include LogicLib.nsh
!include FileFunc.nsh
!include nsDialogs.nsh

Var UpdateMode
Var PassiveMode
Var DeleteAppDataCheckbox
Var DeleteAppDataCheckboxState
Var WebView2Missing   ; 1 = WebView2 still missing after setup: the Finish page says so (OQ-14)

!include "${SR_HOOKS}"
!include "${SR_FILES}"

; ---- Pages ---------------------------------------------------------------------------------
!define MUI_ABORTWARNING
!define MUI_WELCOMEPAGE_TITLE "Install ${PRODUCTNAME}"
!define MUI_WELCOMEPAGE_TEXT "${PRODUCTNAME} lets Claude and other AI apps search the folders you choose, privately on this computer.$\r$\n$\r$\nIt installs just for you and does not need administrator rights.$\r$\n$\r$\nClick Install to continue."
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_INSTFILES
!ifndef SR_STUB_APP
  !define MUI_FINISHPAGE_RUN
  !define MUI_FINISHPAGE_RUN_TEXT "Open ${PRODUCTNAME}"
  !define MUI_FINISHPAGE_RUN_FUNCTION RunMainBinary
!endif
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!define MUI_PAGE_CUSTOMFUNCTION_SHOW FinishShow
!insertmacro MUI_PAGE_FINISH

; Uninstall: confirm page with one plain checkbox (unticked = keep the index), then progress.
!define MUI_PAGE_CUSTOMFUNCTION_SHOW un.ConfirmShow
!define MUI_PAGE_CUSTOMFUNCTION_LEAVE un.ConfirmLeave
!define MUI_PAGE_CUSTOMFUNCTION_PRE un.SkipIfPassive
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Function SkipIfPassive
  ${If} $PassiveMode = 1
    Abort
  ${EndIf}
FunctionEnd

Function un.SkipIfPassive
  ${If} $PassiveMode = 1
    Abort
  ${EndIf}
FunctionEnd

; WebView2 (the app window's engine) ships with Windows 11 and with Edge on most Windows 10
; machines. When it is missing, the bundled Microsoft installer adds it silently for this user (no
; admin prompt). The standalone runtime installer (SR_WEBVIEW2_OFFLINE) needs no internet; the
; bootstrapper (SR_WEBVIEW2) downloads it. Search from Claude works without WebView2, so a failure
; is never fatal: the Finish page then says what to do (OQ-14).
!define WV2_CLIENT "Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}"

; $0 = installed WebView2 version, or "" when it is missing.
Function WebView2Version
  ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\${WV2_CLIENT}" "pv"
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    ReadRegStr $0 HKLM "SOFTWARE\${WV2_CLIENT}" "pv"
  ${EndIf}
  ${If} $0 == ""
  ${OrIf} $0 == "0.0.0.0"
    ReadRegStr $0 HKCU "Software\${WV2_CLIENT}" "pv"
  ${EndIf}
  ${If} $0 == "0.0.0.0"
    StrCpy $0 ""
  ${EndIf}
FunctionEnd

!ifdef SR_WEBVIEW2_OFFLINE
  !define SR_WV2_FILE "MicrosoftEdgeWebView2RuntimeInstallerX64.exe"
  !define SR_WV2_SRC "${SR_WEBVIEW2_OFFLINE}"
!else ifdef SR_WEBVIEW2
  !define SR_WV2_FILE "MicrosoftEdgeWebview2Setup.exe"
  !define SR_WV2_SRC "${SR_WEBVIEW2}"
!endif

Function InstallWebView2IfMissing
  StrCpy $WebView2Missing 0
  Call WebView2Version
  ${If} $0 != ""
    DetailPrint "Microsoft Edge WebView2 is already installed ($0)."
    Return
  ${EndIf}
  !ifdef SR_WV2_FILE
    DetailPrint "Installing Microsoft Edge WebView2 (needed to show the app window)..."
    InitPluginsDir
    File "/oname=$PLUGINSDIR\${SR_WV2_FILE}" "${SR_WV2_SRC}"
    nsExec::ExecToLog '"$PLUGINSDIR\${SR_WV2_FILE}" /silent /install'
    Pop $1
    Delete "$PLUGINSDIR\${SR_WV2_FILE}"
    Call WebView2Version
    ${If} $0 == ""
      DetailPrint "WebView2 setup finished with code $1; the app window needs it, search in Claude works without it."
      StrCpy $WebView2Missing 1
    ${EndIf}
  !else
    StrCpy $WebView2Missing 1
  !endif
FunctionEnd

; Finish page: when WebView2 couldn't be installed, say so in plain words and don't offer to open
; a window that can't show (OQ-14 copy, page 02).
Function FinishShow
  ${If} $WebView2Missing = 1
    !ifdef SR_WEBVIEW2_OFFLINE
      ; The runtime is bundled: no internet involved, so don't blame the connection.
      SendMessage $mui.FinishPage.Text ${WM_SETTEXT} 0 "STR:${PRODUCTNAME} needs Microsoft Edge WebView2 to show its window, and it couldn't be installed. Restart your computer and run this installer again."
    !else
      SendMessage $mui.FinishPage.Text ${WM_SETTEXT} 0 "STR:${PRODUCTNAME} needs Microsoft Edge WebView2 to show its window, and it couldn't be installed without an internet connection. Connect to the internet and run this installer again."
    !endif
    !ifndef SR_STUB_APP
      SendMessage $mui.FinishPage.Run ${BM_SETCHECK} 0 0 ; unchecked
    !endif
  ${EndIf}
FunctionEnd

!ifndef SR_STUB_APP
Function RunMainBinary
  Exec '"$INSTDIR\${MAINBINARYNAME}.exe"'
FunctionEnd
!endif

Function un.ConfirmShow
  FindWindow $1 "#32770" "" $HWNDPARENT
  System::Call "user32::GetDpiForWindow(p r1) i .r2"
  ${If} $2 = 0
    StrCpy $2 96
  ${EndIf}
  IntOp $4 0 * $2
  IntOp $5 100 * $2
  IntOp $6 420 * $2
  IntOp $7 25 * $2
  IntOp $4 $4 / 96
  IntOp $5 $5 / 96
  IntOp $6 $6 / 96
  IntOp $7 $7 / 96
  System::Call 'user32::CreateWindowEx(i ${__NSD_CheckBox_EXSTYLE}, w "${__NSD_CheckBox_CLASS}", w "Also delete my search index (your own files are never touched)", i ${__NSD_CheckBox_STYLE}, i r4, i r5, i r6, i r7, p r1, i0, i0, i0) p .s'
  Pop $DeleteAppDataCheckbox
  SendMessage $HWNDPARENT ${WM_GETFONT} 0 0 $1
  SendMessage $DeleteAppDataCheckbox ${WM_SETFONT} $1 1
FunctionEnd

Function un.ConfirmLeave
  SendMessage $DeleteAppDataCheckbox ${BM_GETCHECK} 0 0 $DeleteAppDataCheckboxState
FunctionEnd

; ---- Install -------------------------------------------------------------------------------
Function .onInit
  StrCpy $UpdateMode 0
  StrCpy $PassiveMode 0
  StrCpy $WebView2Missing 0
  ${GetOptions} $CMDLINE "/P" $R0
  ${IfNot} ${Errors}
    StrCpy $PassiveMode 1
  ${EndIf}
  ClearErrors
  ${GetOptions} $CMDLINE "/UPDATE" $R0
  ${IfNot} ${Errors}
    StrCpy $UpdateMode 1
  ${EndIf}
FunctionEnd

Section Install
  SetOutPath "$INSTDIR"
  !insertmacro NSIS_HOOK_PREINSTALL

  !insertmacro SR_INSTALL_FILES
  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; Apps & features entry (HKCU: per-user, no admin).
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayName" "${PRODUCTNAME}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayVersion" "${SR_VERSION}"
  WriteRegStr HKCU "${UNINSTKEY}" "Publisher" "${MANUFACTURER}"
  WriteRegStr HKCU "${UNINSTKEY}" "DisplayIcon" "$\"$INSTDIR\${MAINBINARYNAME}.exe$\""
  WriteRegStr HKCU "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINSTKEY}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteRegStr HKCU "${UNINSTKEY}" "QuietUninstallString" "$\"$INSTDIR\uninstall.exe$\" /S"
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINSTKEY}" "NoRepair" 1
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINSTKEY}" "EstimatedSize" "$0"

  ${If} $UpdateMode <> 1
    CreateShortcut "$SMPROGRAMS\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
  ${EndIf}

  Call InstallWebView2IfMissing

  !insertmacro NSIS_HOOK_POSTINSTALL

  ${If} $PassiveMode = 1
    SetAutoClose true
  ${EndIf}
SectionEnd

; ---- Uninstall -----------------------------------------------------------------------------
Function un.onInit
  StrCpy $UpdateMode 0
  StrCpy $PassiveMode 0
  StrCpy $DeleteAppDataCheckboxState 0
  ${GetOptions} $CMDLINE "/P" $R0
  ${IfNot} ${Errors}
    StrCpy $PassiveMode 1
  ${EndIf}
  ClearErrors
  ${GetOptions} $CMDLINE "/UPDATE" $R0
  ${IfNot} ${Errors}
    StrCpy $UpdateMode 1
  ${EndIf}
FunctionEnd

Section Uninstall
  !insertmacro NSIS_HOOK_PREUNINSTALL

  !insertmacro SR_UNINSTALL_FILES
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  ${If} $UpdateMode <> 1
    Delete "$SMPROGRAMS\${PRODUCTNAME}.lnk"
    ; Autostart value written by sr-app (tauri-plugin-autostart uses the product name).
    DeleteRegValue HKCU "${RUNKEY}" "${PRODUCTNAME}"
  ${EndIf}
  DeleteRegKey HKCU "${UNINSTKEY}"

  !insertmacro NSIS_HOOK_POSTUNINSTALL

  ${If} $PassiveMode = 1
  ${OrIf} $UpdateMode = 1
    SetAutoClose true
  ${EndIf}
SectionEnd
