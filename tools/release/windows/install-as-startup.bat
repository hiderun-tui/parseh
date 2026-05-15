@echo off
:: Add parseh-miner.exe to Windows startup via a shortcut in the user's
:: Startup folder. No registry edits, no admin rights needed.

setlocal

set "EXE=%~dp0parseh-miner.exe"
set "STARTUP=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup"
set "SHORTCUT=%STARTUP%\parseh-miner.lnk"

if not exist "%EXE%" (
  echo ERROR: parseh-miner.exe not found in %~dp0
  echo This script must live in the same folder as parseh-miner.exe.
  pause
  exit /b 1
)

if not exist "%STARTUP%" (
  echo ERROR: Startup folder not found at %STARTUP%
  pause
  exit /b 1
)

:: Build the shortcut using a PowerShell one-liner.
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$ws = New-Object -ComObject WScript.Shell;" ^
  "$s = $ws.CreateShortcut('%SHORTCUT%');" ^
  "$s.TargetPath = '%EXE%';" ^
  "$s.WorkingDirectory = '%~dp0';" ^
  "$s.Description = 'PARSEH miner - volunteer node for the open censorship-resistant network';" ^
  "$s.WindowStyle = 7;" ^
  "$s.Save()"

if errorlevel 1 (
  echo ERROR: Failed to create shortcut.
  pause
  exit /b 1
)

echo.
echo OK: PARSEH miner will start at Windows login.
echo Shortcut: %SHORTCUT%
echo To remove, run uninstall.bat from this folder.
echo.
pause
