@echo off
setlocal
cd /d "%~dp0"

if not exist "cpe_test.exe" (
  echo [错误] 未找到 cpe_test.exe。请确认本脚本与程序在同一目录。
  pause
  exit /b 1
)

echo 正在启动辅测机 Agent（端口 28801）...
echo 请保持此窗口打开；首次运行的防火墙提示请选择“允许访问”。
echo.
cpe_test.exe agent
echo.
echo Agent 已停止。
pause
