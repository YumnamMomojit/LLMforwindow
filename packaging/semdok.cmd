@echo off
rem Semdok Edge launcher: runs semdok-edge.exe (next to this file) with your arguments.
rem semdok-edge.exe finds Python and the Colibri engine itself (a "colibri" folder here, the py
rem launcher, python); SEMDOK_PYTHON / SEMDOK_COLIBRI_DIR or --python / --colibri-dir override that.
rem   semdok.cmd check     shows what was found and what is missing
rem   semdok.cmd --help    lists every command
rem Delayed expansion stays off even if the registry turns it on: it would eat every ! in an
rem argument (--system "Be brief!").
setlocal EnableExtensions DisableDelayedExpansion
if "%~1"=="" goto :no_args
rem After Ctrl+C, cmd may ask "Terminate batch job (Y/N)?" once semdok-edge.exe has finished; either answer is fine.
"%~dp0semdok-edge.exe" %*
exit /b %ERRORLEVEL%

rem Started without a command, e.g. by a double-click: show the help and keep the window open.
:no_args
"%~dp0semdok-edge.exe" --help
echo.
echo Run semdok.cmd from a Command Prompt opened in this folder; start with: semdok.cmd check
echo (README-FIRST.txt explains every step.)
pause
exit /b 0
