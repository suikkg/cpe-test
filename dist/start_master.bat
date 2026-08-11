@echo off
setlocal
cd /d "%~dp0"

if not exist "cpe_test.exe" (
  echo [错误] 未找到 cpe_test.exe。请确认本脚本与程序在同一目录。
  pause
  exit /b 1
)

set "CONFIG=%~1"
if "%CONFIG%"=="" set "CONFIG=configs\config-sgmii.json"
if not exist "%CONFIG%" (
  echo [错误] 未找到配置文件：%CONFIG%
  echo 用法：start_master.bat configs\config-sgmii.json
  pause
  exit /b 1
)

set /p AGENT_HOST=请输入辅测机 IP：
if "%AGENT_HOST%"=="" (
  echo [错误] 辅测机 IP 不能为空。
  pause
  exit /b 1
)

echo.
echo 使用配置：%CONFIG%
cpe_test.exe master --agent-host %AGENT_HOST% --config "%CONFIG%"
echo.
pause
