# Dev-only: answers the real Windows "Choose a folder" dialog (opened by the app's folder picker)
# with window messages, so no keyboard focus is needed: the path is typed into the dialog's
# "Folder:" box character by character (WM_CHAR), Enter navigates into it, then "Select Folder". -Cancel presses Cancel (IDCANCEL) on
# every open "Choose a folder" dialog.
# Usage: powershell -NoProfile -File answer-folder-dialog.ps1 -Path <folder> | -Cancel
param([string]$Path = "", [switch]$Cancel, [int]$TimeoutSec = 30)
Add-Type @"
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
public static class Dlg {
  delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc f, IntPtr l);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern int GetClassName(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr FindWindowEx(IntPtr p, IntPtr after, string cls, string title);
  [DllImport("user32.dll")] static extern IntPtr GetDlgItem(IntPtr h, int id);
  [DllImport("user32.dll")] static extern bool EnumChildWindows(IntPtr p, EnumProc f, IntPtr l);
  [DllImport("user32.dll")] static extern IntPtr GetParent(IntPtr h);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] static extern IntPtr SendMessage(IntPtr h, uint msg, IntPtr w, string l);
  [DllImport("user32.dll")] static extern IntPtr SendMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
  public static List<IntPtr> Find(string title) {
    var r = new List<IntPtr>();
    EnumWindows((h, l) => {
      var t = new StringBuilder(256); GetWindowText(h, t, 256);
      var c = new StringBuilder(64); GetClassName(h, c, 64);
      if (IsWindowVisible(h) && t.ToString() == title && c.ToString() == "#32770") r.Add(h);
      return true;
    }, IntPtr.Zero);
    return r;
  }
  public static bool Choose(IntPtr dlg, string path) {
    // The "Folder:" box is an Edit inside a ComboBox, several levels below the dialog.
    IntPtr edit = IntPtr.Zero;
    EnumChildWindows(dlg, (h, l) => {
      var c = new StringBuilder(64); GetClassName(h, c, 64);
      var pc = new StringBuilder(64); GetClassName(GetParent(h), pc, 64);
      if (c.ToString() == "Edit" && pc.ToString() == "ComboBox") { edit = h; return false; }
      return true;
    }, IntPtr.Zero);
    if (edit == IntPtr.Zero) return false;
    // Type it (WM_CHAR per character, like a keyboard; the dialog sees the change), then Enter.
    SendMessage(edit, 0x000C /* WM_SETTEXT */, IntPtr.Zero, "");
    foreach (char ch in path) SendMessage(edit, 0x0102 /* WM_CHAR */, (IntPtr)ch, IntPtr.Zero);
    System.Threading.Thread.Sleep(200);
    PostMessage(edit, 0x0100 /* WM_KEYDOWN */, (IntPtr)0x0D, IntPtr.Zero);
    PostMessage(edit, 0x0102 /* WM_CHAR */, (IntPtr)0x0D, IntPtr.Zero);
    PostMessage(edit, 0x0101 /* WM_KEYUP */, (IntPtr)0x0D, IntPtr.Zero);
    // Enter navigates into the folder; "Select Folder" (IDOK) then picks it.
    System.Threading.Thread.Sleep(1000);
    SendMessage(GetDlgItem(dlg, 1 /* IDOK */), 0x00F5 /* BM_CLICK */, IntPtr.Zero, IntPtr.Zero);
    return true;
  }
  public static void Cancel(IntPtr dlg) {
    SendMessage(GetDlgItem(dlg, 2 /* IDCANCEL */), 0x00F5, IntPtr.Zero, IntPtr.Zero);
  }
}
"@
$deadline = (Get-Date).AddSeconds($TimeoutSec)
do {
  $dlgs = [Dlg]::Find("Choose a folder")
  if ($dlgs.Count -gt 0 -or $Cancel) { break }
  Start-Sleep -Milliseconds 250
} while ((Get-Date) -lt $deadline)
if ($Cancel) { foreach ($d in $dlgs) { [Dlg]::Cancel($d) }; "cancelled $($dlgs.Count)"; exit 0 }
if ($dlgs.Count -eq 0) { Write-Error "dialog not found"; exit 2 }
Start-Sleep -Milliseconds 500   # let the dialog finish building its controls
if (-not [Dlg]::Choose($dlgs[0], $Path)) { Write-Error "Folder: box not found"; exit 3 }
"selected $Path"
