; Hooks into Tauri's NSIS installer (bundle > windows > nsis > installerHooks).
;
; «Start with Windows» (tauri-plugin-autostart) writes a per-user Run value named
; after the product, and a StartupApproved value beside it. Tauri's uninstaller
; already removes the Run value but leaves the StartupApproved one; this hook
; removes both, the first again only as a safeguard. An ordinary update does not
; uninstall the previous version. Should an uninstaller ever run with /UPDATE
; ($UpdateMode 1), both values stay, so the setting survives.

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "${PRODUCTNAME}"
  ${EndIf}
!macroend
