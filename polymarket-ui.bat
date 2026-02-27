@echo off
setlocal

REM Launch Polymarket copy-trading local web UI on Windows (double-click friendly)
REM Usage (optional): polymarket-ui.bat [host] [port]

set HOST=%~1
if "%HOST%"=="" set HOST=127.0.0.1

set PORT=%~2
if "%PORT%"=="" set PORT=8787

where polymarket >nul 2>nul
if %ERRORLEVEL% neq 0 (
  echo [ERROR] No se encontro el ejecutable "polymarket" en PATH.
  echo Instala la CLI primero o agrega la ruta al PATH.
  echo.
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
