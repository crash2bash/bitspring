; BitSpring was called Hifi up to 0.4.1, and Tauri's installer keys an install by the product
; name: without this, updating would put BitSpring next to the old Hifi instead of replacing it.
; The old uninstaller runs silently and without /UPDATE, so it also takes the Hifi shortcuts with
; it; it never touches the settings, library and playlists, which live under the app identifier
; (unchanged, dev.local.hifi) and carry over to BitSpring as they are.
!macro NSIS_HOOK_PREINSTALL
  ReadRegStr $R9 HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hifi" "InstallLocation"
  ${If} $R9 != ""
    ; Stored with quotes around it.
    StrCpy $R9 $R9 -1 1
    ${If} ${FileExists} "$R9\uninstall.exe"
      ; _?= runs the uninstaller in place, so ExecWait really waits for it.
      ExecWait '"$R9\uninstall.exe" /S _?=$R9'
      Delete "$R9\uninstall.exe"
      RMDir "$R9"
    ${EndIf}
    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\Hifi"
  ${EndIf}
!macroend
