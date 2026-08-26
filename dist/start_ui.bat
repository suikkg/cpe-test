@echo off
setlocal
cd /d "%~dp0"

if not exist "cpe_test.exe" (
  echo [错误] 未找到 cpe_test.exe。请确认本脚本与程序在同一目录。
  pause
  exit /b 1
)

echo 正在启动图形控制台...
echo 浏览器会自动打开 http://127.0.0.1:28800
echo 没弹出的话，手动复制上面这个地址到浏览器。
echo 保持此窗口打开；关掉它控制台就停了。
echo.
cpe_test.exe ui
echo.
echo 控制台已停止。
pause
