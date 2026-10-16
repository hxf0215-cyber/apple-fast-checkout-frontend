@echo off
setlocal

set "ROOT=%~dp0"
set "EXE=%ROOT%target\release\apple-fast-checkout.exe"
set "GUARD=%ROOT%start-guarded.ps1"
set "POWERSHELL=%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe"
set "URL=http://127.0.0.1:8080"

if not exist "%EXE%" (
  echo [ERROR] Backend not found: %EXE%
  echo Please copy the entire apple folder, or run: cargo build --release
  pause
  exit /b 1
)

if not exist "%GUARD%" (
  echo [ERROR] Guard script not found: %GUARD%
  pause
  exit /b 1
)

if not exist "%ROOT%static" (
  echo [ERROR] static folder not found: %ROOT%static
  pause
  exit /b 1
)

tasklist /FI "IMAGENAME eq apple-fast-checkout.exe" 2>NUL | find /I "apple-fast-checkout.exe" >NUL
if %ERRORLEVEL% EQU 0 (
  echo Fast Checkout is already running.
) else (
  echo Starting Fast Checkout with auto-restart guard...
  start "Fast Checkout Guard" /min "%POWERSHELL%" -NoProfile -ExecutionPolicy Bypass -File "%GUARD%"
)

echo Waiting for service...
"%POWERSHELL%" -NoProfile -ExecutionPolicy Bypass -Command ^
  "$ready = $false; for ($i = 0; $i -lt 30; $i++) { try { $response = Invoke-WebRequest -UseBasicParsing -Uri 'http://127.0.0.1:8080/api/state' -TimeoutSec 1; if ($response.StatusCode -eq 200) { $ready = $true; break } } catch { Start-Sleep -Milliseconds 500 } }; if (-not $ready) { exit 1 }"
if errorlevel 1 (
  echo [ERROR] Service did not start within 15 seconds.
  echo Check logs\service-guard.log and logs\ for details.
  pause
  exit /b 1
)

echo Fast Checkout is ready: %URL%
start "" "%URL%"
exit /b 0
