@echo off
setlocal EnableExtensions

REM Double-click launcher for Polymarket copy UI on Windows
REM Usage: polymarket-ui.bat [host] [port]

set "HOST=%~1"
if "%HOST%"=="" set "HOST=127.0.0.1"

set "PORT=%~2"
if "%PORT%"=="" set "PORT=8787"

set "SCRIPT_DIR=%~dp0"
set "INSTALL_ROOT=%LocalAppData%\Programs\Polymarket"
set "BIN_DIR=%INSTALL_ROOT%\bin"
set "LOCAL_EXE=%BIN_DIR%\polymarket.exe"
set "CLI_CMD="

echo =========================================
echo Polymarket UI Launcher
echo Host: %HOST%
echo Port: %PORT%
echo =========================================

REM 1) Resolve from PATH first
where polymarket >nul 2>nul
if not errorlevel 1 set "CLI_CMD=polymarket"

REM 2) Resolve from local install dir
if "%CLI_CMD%"=="" if exist "%LOCAL_EXE%" set "CLI_CMD=%LOCAL_EXE%"

REM 3) Try auto-install if still missing
if not "%CLI_CMD%"=="" goto :run_ui

echo [INFO] polymarket.exe no encontrado. Intentando instalar...

if exist "%SCRIPT_DIR%polymarket.exe" goto :install_from_local_exe

where cargo >nul 2>nul
if errorlevel 1 goto :fail_missing

echo [INFO] Compilando e instalando CLI con cargo...
pushd "%SCRIPT_DIR%" >nul
cargo install --path . --locked --root "%INSTALL_ROOT%"
set "INSTALL_ERR=%ERRORLEVEL%"
popd >nul
if not "%INSTALL_ERR%"=="0" goto :fail_install

if exist "%LOCAL_EXE%" (
  set "CLI_CMD=%LOCAL_EXE%"
) else (
  where polymarket >nul 2>nul
  if not errorlevel 1 set "CLI_CMD=polymarket"
)

if "%CLI_CMD%"=="" goto :fail_not_resolved
goto :run_ui

:install_from_local_exe
if not exist "%BIN_DIR%" mkdir "%BIN_DIR%" >nul 2>nul
copy /Y "%SCRIPT_DIR%polymarket.exe" "%LOCAL_EXE%" >nul
if errorlevel 1 goto :fail_copy
set "CLI_CMD=%LOCAL_EXE%"
echo [OK] Se copio polymarket.exe en "%BIN_DIR%".

goto :run_ui

:run_ui
if not exist "%BIN_DIR%" goto :run_launch
set "PATH=%PATH%;%BIN_DIR%"
setx PATH "%PATH%" >nul 2>nul

:run_launch
echo [INFO] Abriendo consola interactiva de Polymarket (shell)...
start "Polymarket CLI" cmd /k ""%CLI_CMD%" shell"

echo [INFO] Se abrira el navegador en http://%HOST%:%PORT% (tras 2s)...
start "" "http://%HOST%:%PORT%"

echo [INFO] Ejecutando: "%CLI_CMD%" copy ui --host %HOST% --port %PORT%
echo.
"%CLI_CMD%" copy ui --host %HOST% --port %PORT%
set "RUN_ERR=%ERRORLEVEL%"
if not "%RUN_ERR%"=="0" goto :fail_run

echo.
echo [OK] La UI termino correctamente.
goto :end

:fail_copy
echo.
echo [ERROR] No se pudo copiar polymarket.exe a "%BIN_DIR%".
goto :end

:fail_missing
echo.
echo [ERROR] No se encontro polymarket.exe junto al .bat y tampoco cargo para instalar.
echo         Coloca polymarket.exe junto a este launcher o instala Rust/Cargo.
goto :end

:fail_install
echo.
echo [ERROR] Fallo la instalacion automatica de polymarket CLI.
goto :end

:fail_not_resolved
echo.
echo [ERROR] No se pudo resolver el ejecutable de Polymarket.
goto :end

:fail_run
echo.
echo [ERROR] La UI termino con error (code %RUN_ERR%).

:end
echo.
pause
endlocal
