@echo off
rem Local AI Hub launcher. Finds Python and the Colibri engine, then runs hubd.exe with your
rem arguments. HUB_PYTHON / HUB_COLIBRI_DIR (or --python / --colibri-dir) always win.
rem   hub.cmd check     shows what was found
rem   hub.cmd --help    lists every command
rem No parenthesised blocks around paths: "C:\Program Files (x86)\..." would end them early.
rem Delayed expansion stays off even if the registry turns it on: it would eat every ! in a
rem path or argument (--system "Be brief!").
setlocal EnableExtensions DisableDelayedExpansion
set "HUB_HOME=%~dp0"
set "HUB_SKIPPED_PTH="

if defined HUB_PYTHON goto :have_python
rem A python folder next to hub.cmd is used only when it holds a full install. The embeddable
rem package has a python3XX._pth file beside python.exe, which keeps openai_server.py from
rem importing the files next to it, so the engine could not start. pushd + a bare pattern instead
rem of a quoted "path\*" set, which some cmd implementations do not expand.
if not exist "%HUB_HOME%python\python.exe" goto :find_python
pushd "%HUB_HOME%python"
for %%F in (*._pth) do set "HUB_SKIPPED_PTH=%%F"
popd
if not defined HUB_SKIPPED_PTH set "HUB_PYTHON=%HUB_HOME%python\python.exe"
if defined HUB_PYTHON goto :have_python
:find_python
rem The py launcher first; a bare "python" can be the Microsoft Store stub, which prints nothing.
for /f "usebackq delims=" %%P in (`py -3 -c "import sys; assert sys.version_info >= (3, 10); print(sys.executable)" 2^>nul`) do set "HUB_PYTHON=%%P"
if defined HUB_PYTHON goto :have_python
for /f "usebackq delims=" %%P in (`python -c "import sys; assert sys.version_info >= (3, 10); print(sys.executable)" 2^>nul`) do set "HUB_PYTHON=%%P"
:have_python

if defined HUB_COLIBRI_DIR goto :have_colibri
rem The official release zip is flat (openai_server.py next to the engines); a source tree has c\.
if exist "%HUB_HOME%colibri\openai_server.py" set "HUB_COLIBRI_DIR=%HUB_HOME%colibri"
if defined HUB_COLIBRI_DIR goto :have_colibri
if exist "%HUB_HOME%colibri\c\openai_server.py" set "HUB_COLIBRI_DIR=%HUB_HOME%colibri"
if defined HUB_COLIBRI_DIR goto :have_colibri
rem Unzipped with its own top-level folder: colibri\<folder>\openai_server.py.
if not exist "%HUB_HOME%colibri\" goto :have_colibri
pushd "%HUB_HOME%colibri"
for /d %%D in (*) do if exist "%%~fD\openai_server.py" set "HUB_COLIBRI_DIR=%%~fD"
popd
:have_colibri

if /i "%~1"=="check" goto :check
if "%~1"=="" goto :no_args
rem After Ctrl+C, cmd may ask "Terminate batch job (Y/N)?" once hubd.exe has finished; either answer is fine.
"%HUB_HOME%hubd.exe" %*
exit /b %ERRORLEVEL%

rem Started without a command, e.g. by a double-click: show the help and keep the window open.
:no_args
"%HUB_HOME%hubd.exe" --help
echo.
echo Run hub.cmd from a Command Prompt opened in this folder; start with: hub.cmd check
echo (README-FIRST.txt explains every step.)
pause
exit /b 0

:check
echo Local AI Hub setup check
set "HUB_CHECK_FAILED="
if exist "%HUB_HOME%hubd.exe" goto :have_hubd
echo   hubd.exe : NOT FOUND next to hub.cmd. Unzip the whole Local AI Hub folder again.
set "HUB_CHECK_FAILED=1"
goto :check_python
:have_hubd
echo   hubd.exe : "%HUB_HOME%hubd.exe"
:check_python
if not defined HUB_SKIPPED_PTH goto :check_python_found
echo   note     : "%HUB_HOME%python\python.exe" is NOT used: it is the embeddable Python
echo              (%HUB_SKIPPED_PTH%), which cannot run the engine. Delete that python folder.
:check_python_found
if not defined HUB_PYTHON goto :no_python
echo   Python   : "%HUB_PYTHON%"
rem Run it: a broken or too old Python would otherwise only fail when serving starts.
"%HUB_PYTHON%" -c "import sys; assert sys.version_info >= (3, 10); print('             version ' + sys.version.split()[0])"
if not errorlevel 1 goto :check_python_path
echo              does NOT run, or is older than 3.10. README-FIRST.txt step 1.
set "HUB_CHECK_FAILED=1"
goto :check_colibri
:check_python_path
rem The engine's openai_server.py imports the files next to it, which needs the script's own
rem folder on sys.path ('' for -c). A ._pth file (embeddable package) or PYTHONSAFEPATH drops it.
"%HUB_PYTHON%" -c "import sys; sys.exit(sys.path[:1] != [''])"
if not errorlevel 1 goto :check_colibri
echo              cannot run the engine: it does not import a script's own folder (an
echo              embeddable Python with a ._pth file, or PYTHONSAFEPATH is set). Install
echo              Python from python.org instead. README-FIRST.txt step 1.
set "HUB_CHECK_FAILED=1"
goto :check_colibri
:no_python
echo   Python   : NOT FOUND. Install Python 3.10 or newer from https://www.python.org/downloads/
echo              (tick "Add python.exe to PATH"), then run hub.cmd check again. README-FIRST.txt step 1.
set "HUB_CHECK_FAILED=1"
:check_colibri
if not defined HUB_COLIBRI_DIR goto :no_colibri
echo   Colibri  : "%HUB_COLIBRI_DIR%"
if exist "%HUB_COLIBRI_DIR%\openai_server.py" goto :check_done
if exist "%HUB_COLIBRI_DIR%\c\openai_server.py" goto :check_done
echo              has no openai_server.py. README-FIRST.txt step 2.
set "HUB_CHECK_FAILED=1"
goto :check_done
:no_colibri
echo   Colibri  : NOT FOUND. Unzip the official Colibri v2.0.0 Windows release into
echo              "%HUB_HOME%colibri" so that openai_server.py is inside it. README-FIRST.txt step 2.
set "HUB_CHECK_FAILED=1"
:check_done
if defined HUB_CHECK_FAILED exit /b 1
echo Ready. Next: hub.cmd init, then add a model (README-FIRST.txt step 3).
exit /b 0
