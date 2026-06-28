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

npm start
endlocal
