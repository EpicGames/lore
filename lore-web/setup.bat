@echo off
REM Set up lore-web: check and install its dependencies, then install the SDK.
REM Safe to re-run. Works from anywhere (uses the script's own folder).
REM
REM Networking (VPN/tunnel/port forwarding to reach a server) is intentionally
REM out of scope here -- arrange that yourself.
setlocal
cd /d "%~dp0"

echo [lore-web] Checking dependencies...

REM --- Node.js (required) ---
where node >nul 2>nul
if errorlevel 1 goto :no_node
goto :have_node

:no_node
echo [lore-web] Node.js was not found.
where winget >nul 2>nul
if errorlevel 1 (
  echo            Install Node.js 18-24 from https://nodejs.org/ and re-run setup.bat.
  exit /b 1
)
choice /C YN /M "[lore-web] Install Node.js LTS via winget now"
if errorlevel 2 (
  echo            Install Node.js 18-24 from https://nodejs.org/ and re-run setup.bat.
  exit /b 1
)
winget install -e --id OpenJS.NodeJS.LTS --accept-source-agreements --accept-package-agreements
echo.
echo [lore-web] Node.js installed. Close this window, open a new one, and run
echo            setup.bat again so the updated PATH takes effect.
exit /b 0

:have_node
echo [lore-web] Installing npm dependencies ^(npm install^)...
call npm install
if errorlevel 1 (
  echo [lore-web] npm install failed. Check your internet connection and try again.
  exit /b 1
)

REM --- lore CLI (needed to log in to a server) ---
where lore >nul 2>nul
if errorlevel 1 goto :no_lore
goto :done

:no_lore
echo [lore-web] The 'lore' CLI was not found.
choice /C YN /M "[lore-web] Install it now via the official Lore installer"
if errorlevel 2 (
  echo            Install it later from:
  echo              https://epicgames.github.io/lore/how-to/install-lore-cli/
  goto :done
)
powershell -NoProfile -ExecutionPolicy Bypass -Command "irm https://raw.githubusercontent.com/EpicGames/lore/main/scripts/install.ps1 | iex"
echo [lore-web] If 'lore' is still not found, open a new terminal for PATH to update.

:done
echo.
echo [lore-web] Setup complete. Run start.bat ^(or: npm start^) to launch.
echo            Before syncing, make sure this machine can reach your Lore server's host.
endlocal
