; Hindsight runs in the tray; make sure it isn't holding files open.
!macro NSIS_HOOK_PREINSTALL
  nsExec::Exec 'taskkill /F /IM hindsight.exe'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'taskkill /F /IM hindsight.exe'
  ; Remove the "Start with Windows" entry the app writes.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "Hindsight"
!macroend
