@echo off
setlocal
cd /d "%~dp0"

if not exist "cpe_test.exe" (
  echo [错误] 未找到 cpe_test.exe。请确认本脚本与程序在同一目录。
  pause
  exit /b 1
)

rem 主控连过来要报的口令，必须和主控那边 start_ui.bat 里的 AGENT_TOKEN 一致。
set "AGENT_TOKEN=cpetest"

echo 正在启动辅测机 Agent（端口 28801）...
echo 浏览器会自动打开状态页 http://127.0.0.1:28802
echo 那个页面上直接写着要报给主控的 IP，还能看到主控正在让本机做什么。
echo 请保持此窗口打开；首次运行的防火墙提示请选择“允许访问”。
echo.
cpe_test.exe agent --token %AGENT_TOKEN%
echo.
echo Agent 已停止。
pause
