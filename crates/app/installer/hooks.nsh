; Hooks into Tauri's NSIS installer (bundle > windows > nsis > installerHooks).
;
; «Start with Windows» (tauri-plugin-autostart) writes a per-user Run value named
; after the product, and a StartupApproved value beside it. A real uninstall
; removes both, so Windows stops starting a program that is gone. An update runs
; the old uninstaller with /UPDATE ($UpdateMode 1), and there both stay, so the
; setting survives the update.

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "${PRODUCTNAME}"
  ${EndIf}
!macroend
