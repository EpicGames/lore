@echo off
REM Launch lore-web and open it in the default browser.
REM Runs setup automatically on first use.
setlocal
cd /d "%~dp0"

if not exist "node_modules\@lore-vcs\sdk" (
  echo [lore-web] First run - installing dependencies...
  call "%~dp0setup.bat"
  if errorlevel 1 exit /b 1
)

REM Stop any instance already running so a restart always loads current code.
if "%LORE_WEB_PORT%"=="" (set "LWPORT=7420") else (set "LWPORT=%LORE_WEB_PORT%")
echo [lore-web] Stopping any instance on port %LWPORT%...
powershell -NoProfile -Command "Get-NetTCPConnection -LocalPort %LWPORT% -State Listen -ErrorAction SilentlyContinue | Select-Object -ExpandProperty OwningProcess -Unique | ForEach-Object { Stop-Process -Id $_ -Force -ErrorAction SilentlyContinue }"

npm start
endlocal
