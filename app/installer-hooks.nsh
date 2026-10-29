; Hindsight runs in the tray; make sure it isn't holding files open. taskkill
; returns before Windows has released the exe and DLLs, so wait a moment, or
; NSIS silently skips the locked files and the update doesn't land.
!macro NSIS_HOOK_PREINSTALL
  nsExec::Exec 'taskkill /F /IM hindsight.exe'
  Sleep 1500
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'taskkill /F /IM hindsight.exe'
  Sleep 1500
  ; Remove the "Start with Windows" entry the app writes.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Hindsight"
!macroend
