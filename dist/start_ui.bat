@echo off
setlocal
cd /d "%~dp0"

if not exist "cpe_test.exe" (
  echo [错误] 未找到 cpe_test.exe。请确认本脚本与程序在同一目录。
  pause
  exit /b 1
)

rem 口令。两处都用它：UI_TOKEN 是浏览器打开控制台要输的，AGENT_TOKEN 是主控
rem 连辅测机用的，页面上的 token 框留空时沿用它。改成你自己的值。
set "UI_TOKEN=cpetest"
set "AGENT_TOKEN=cpetest"

rem 控制台监听地址。
rem   127.0.0.1  只有本机能打开（最安全，默认）
rem   0.0.0.0    同网段的别的电脑也能打开——口令泄露即等于测试控制权泄露
set "UI_BIND=0.0.0.0"

echo 正在启动图形控制台...
echo 监听地址：%UI_BIND%:28800
if /i not "%UI_BIND%"=="127.0.0.1" (
  echo 从别的电脑访问：http://本机测试网IP:28800?token=%UI_TOKEN%
  echo 首次运行的防火墙提示请选择“允许访问”。
)
echo 保持此窗口打开；关掉它控制台就停了。
echo.
cpe_test.exe ui --ui-bind %UI_BIND% --ui-token %UI_TOKEN% --token %AGENT_TOKEN%
echo.
echo 控制台已停止。
pause
