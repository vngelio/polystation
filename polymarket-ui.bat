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

where polymarket >nul 2>nul
if %ERRORLEVEL% neq 0 (
  echo [INFO] polymarket.exe no esta en PATH. Intentando instalar automaticamente...

  if exist "%SCRIPT_DIR%polymarket.exe" (
    if not exist "%BIN_DIR%" mkdir "%BIN_DIR%" >nul 2>nul
    copy /Y "%SCRIPT_DIR%polymarket.exe" "%LOCAL_EXE%" >nul
    if %ERRORLEVEL% neq 0 (
      echo [ERROR] No se pudo copiar polymarket.exe a "%BIN_DIR%".
      pause
      exit /b 1
    )
    echo [OK] Copiado polymarket.exe localmente.
  ) else (
    where cargo >nul 2>nul
    if %ERRORLEVEL% neq 0 (
      echo [ERROR] No se encontro polymarket.exe en esta carpeta ni cargo para compilar.
      echo Coloca polymarket.exe junto a este .bat o instala Rust/Cargo y vuelve a intentarlo.
      pause
      exit /b 1
    )

    pushd "%SCRIPT_DIR%" >nul
    cargo install --path . --locked --root "%INSTALL_ROOT%"
    set "INSTALL_ERR=%ERRORLEVEL%"
    popd >nul

    if not "%INSTALL_ERR%"=="0" (
      echo [ERROR] Fallo la instalacion automatica de polymarket CLI.
      pause
      exit /b 1
    )
  )

  echo [INFO] Agregando "%BIN_DIR%" al PATH de usuario...
  echo %PATH% | find /I "%BIN_DIR%" >nul
  if %ERRORLEVEL% neq 0 (
    setx PATH "%PATH%;%BIN_DIR%" >nul
    if %ERRORLEVEL% neq 0 (
      echo [WARN] No se pudo persistir PATH automaticamente.
      echo Agrega manualmente "%BIN_DIR%" a PATH si el comando no se encuentra.
    )
  )

  set "PATH=%PATH%;%BIN_DIR%"
)

where polymarket >nul 2>nul
if %ERRORLEVEL% neq 0 (
  echo [ERROR] polymarket.exe sigue sin estar disponible en PATH.
  pause
  exit /b 1
)

echo Iniciando UI local en http://%HOST%:%PORT%
echo.
polymarket copy ui --host %HOST% --port %PORT%

if %ERRORLEVEL% neq 0 (
  echo.
  echo [ERROR] La UI termino con error.
  pause
)

endlocal
