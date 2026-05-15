@echo off
setlocal
set "SHORTCUT=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\parseh-miner.lnk"
if exist "%SHORTCUT%" (
  del "%SHORTCUT%"
  echo OK: PARSEH miner autostart removed.
) else (
  echo Note: PARSEH miner autostart was not enabled (nothing to remove).
)
pause
