@echo off
setlocal EnableExtensions EnableDelayedExpansion

REM Launch Polymarket copy-trading local web UI on Windows (double-click friendly)
REM Usage (optional): polymarket-ui.bat [host] [port]

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

REM 1) Try system PATH first
where polymarket >nul 2>nul
if %ERRORLEVEL%==0 (
  set "CLI_CMD=polymarket"
)

REM 2) If not in PATH, try known local install path
if "%CLI_CMD%"=="" (
  if exist "%LOCAL_EXE%" (
    set "CLI_CMD=%LOCAL_EXE%"
  )
)

REM 3) If still missing, try auto-install
if "%CLI_CMD%"=="" (
  echo [INFO] polymarket.exe no esta disponible. Intentando instalar automaticamente...

  if exist "%SCRIPT_DIR%polymarket.exe" (
    if not exist "%BIN_DIR%" mkdir "%BIN_DIR%" >nul 2>nul
    copy /Y "%SCRIPT_DIR%polymarket.exe" "%LOCAL_EXE%" >nul
    if %ERRORLEVEL% neq 0 (
      echo [ERROR] No se pudo copiar polymarket.exe a "%BIN_DIR%".
      echo.
      pause
      exit /b 1
    )
    set "CLI_CMD=%LOCAL_EXE%"
    echo [OK] Copiado polymarket.exe localmente.
  ) else (
    where cargo >nul 2>nul
    if %ERRORLEVEL% neq 0 (
      echo [ERROR] No se encontro polymarket.exe junto al launcher ni cargo para compilar.
      echo Coloca polymarket.exe junto a este .bat o instala Rust/Cargo y vuelve a intentarlo.
      echo.
      pause
      exit /b 1
    )

    pushd "%SCRIPT_DIR%" >nul
    cargo install --path . --locked --root "%INSTALL_ROOT%"
    set "INSTALL_ERR=%ERRORLEVEL%"
    popd >nul

    if not "%INSTALL_ERR%"=="0" (
      echo [ERROR] Fallo la instalacion automatica de polymarket CLI.
      echo.
      pause
      exit /b 1
    )

    if exist "%LOCAL_EXE%" (
      set "CLI_CMD=%LOCAL_EXE%"
    )
  )
)

if "%CLI_CMD%"=="" (
  echo [ERROR] No fue posible resolver el ejecutable de Polymarket.
  echo.
  pause
  exit /b 1
)

REM Optional best effort PATH update (no bloquear si falla)
if not exist "%BIN_DIR%" goto RUN_UI

echo [INFO] Intentando registrar "%BIN_DIR%" en PATH de usuario...
for /f "tokens=2,*" %%A in ('reg query "HKCU\Environment" /v Path 2^>nul ^| find /I "Path"') do set "USER_PATH=%%B"
if defined USER_PATH (
  echo !USER_PATH! | find /I "%BIN_DIR%" >nul
  if !ERRORLEVEL! neq 0 (
    setx Path "!USER_PATH!;%BIN_DIR%" >nul 2>nul
  )
) else (
  setx Path "%BIN_DIR%" >nul 2>nul
)

:RUN_UI
echo [INFO] Ejecutando: "%CLI_CMD%" copy ui --host %HOST% --port %PORT%
echo.
"%CLI_CMD%" copy ui --host %HOST% --port %PORT%
set "RUN_ERR=%ERRORLEVEL%"

echo.
if not "%RUN_ERR%"=="0" (
  echo [ERROR] La UI termino con error (code %RUN_ERR%).
) else (
  echo [INFO] La UI finalizo.
)

echo.
pause
endlocal
