@echo off
rem Local AI Hub launcher. Finds Python and the Colibri engine, then runs hubd.exe with your
rem arguments. HUB_PYTHON / HUB_COLIBRI_DIR (or --python / --colibri-dir) always win.
rem   hub.cmd check     shows what was found
rem   hub.cmd --help    lists every command
rem No parenthesised blocks around paths: "C:\Program Files (x86)\..." would end them early.
setlocal EnableExtensions
set "HUB_HOME=%~dp0"

if defined HUB_PYTHON goto :have_python
if exist "%HUB_HOME%python\python.exe" set "HUB_PYTHON=%HUB_HOME%python\python.exe"
if defined HUB_PYTHON goto :have_python
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
rem Unzipped with its own top-level folder: colibri\<folder>\openai_server.py. pushd + a bare *
rem pattern instead of a quoted "path\*" set, which some cmd implementations do not expand.
if not exist "%HUB_HOME%colibri\" goto :have_colibri
pushd "%HUB_HOME%colibri"
for /d %%D in (*) do if exist "%%~fD\openai_server.py" set "HUB_COLIBRI_DIR=%%~fD"
popd
:have_colibri

if /i "%~1"=="check" goto :check
"%HUB_HOME%hubd.exe" %*
exit /b %ERRORLEVEL%

:check
echo Local AI Hub setup check
echo   hubd.exe : "%HUB_HOME%hubd.exe"
if not defined HUB_PYTHON goto :no_python
echo   Python   : "%HUB_PYTHON%"
"%HUB_PYTHON%" --version
goto :check_colibri
:no_python
echo   Python   : NOT FOUND. Install Python 3.10 or newer from https://www.python.org/downloads/
echo              (tick "Add python.exe to PATH"), then run hub.cmd check again. README-FIRST.txt step 1.
:check_colibri
if not defined HUB_COLIBRI_DIR goto :no_colibri
echo   Colibri  : "%HUB_COLIBRI_DIR%"
goto :check_done
:no_colibri
echo   Colibri  : NOT FOUND. Unzip the official Colibri v2.0.0 Windows release into
echo              "%HUB_HOME%colibri" so that openai_server.py is inside it. README-FIRST.txt step 2.
:check_done
if not defined HUB_PYTHON exit /b 1
if not defined HUB_COLIBRI_DIR exit /b 1
echo Ready. Next: hub.cmd init, then add a model (README-FIRST.txt step 3).
exit /b 0
