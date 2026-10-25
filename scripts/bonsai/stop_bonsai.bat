@echo off
REM Deterministic teardown for the Windows-native Bonsai llama-server.exe.
REM Safe to run from anywhere and when nothing is up; leaves no dangling
REM process or file locks (the dynamic -lcd lookup cache file is released
REM when the process terminates).
setlocal

echo =======================================================
echo [stop_bonsai] Terminating llama-server.exe (port 18020)
echo =======================================================

:: 1. Pre-flight check: is llama-server.exe even running?
tasklist /FI "IMAGENAME eq llama-server.exe" /NH 2>nul | findstr /I llama-server.exe >nul
if errorlevel 1 (
    echo [stop_bonsai] No llama-server.exe process found. Nothing to do.
    endlocal
    exit /b 0
)

:: 2. Graceful terminate first (lets the engine flush and release handles).
echo [stop_bonsai] Issuing graceful termination...
taskkill /IM llama-server.exe >nul 2>&1

:: 3. Wait up to 5s for a clean exit.
set /a TRIES=0
:waitloop
timeout /t 1 /nobreak >nul
set /a TRIES+=1
tasklist /FI "IMAGENAME eq llama-server.exe" /NH 2>nul | findstr /I llama-server.exe >nul
if errorlevel 1 goto confirmed
if %TRIES% LSS 5 goto waitloop

:: 4. Force-kill any survivor, then verify.
echo [stop_bonsai] Graceful stop timed out; force-killing...
taskkill /F /IM llama-server.exe >nul 2>&1
timeout /t 1 /nobreak >nul

:confirmed
tasklist /FI "IMAGENAME eq llama-server.exe" /NH 2>nul | findstr /I llama-server.exe >nul
if errorlevel 1 (
    echo [stop_bonsai] Confirmed: llama-server.exe fully stopped, no dangling processes.
    endlocal
    exit /b 0
)

echo [stop_bonsai] [ERROR] llama-server.exe is still alive after force-kill.
endlocal
exit /b 1
