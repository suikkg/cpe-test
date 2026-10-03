@echo off
setlocal
cd /d "%~dp0"

if not exist "cpe_test.exe" (
  echo [错误] 未找到 cpe_test.exe。请确认本脚本与程序在同一目录。
  pause
  exit /b 1
)

rem 连辅测机用的共享令牌：主控与辅测两端必须一致，默认 cpetest，改成你自己的值。
rem 控制台自己的访问口令不在这里设：不写 --ui-token 时，程序每次启动会现生成一枚
rem 随机口令并打印在下面的启动地址里（带 ?token=），浏览器会被自动带着打开。
rem 要固定成自己的值就加一行 --ui-token 你的口令（或设环境变量 CPE_UI_TOKEN）。
set "AGENT_TOKEN=cpetest"

rem 控制台监听地址。
rem   127.0.0.1  只有本机能打开（最安全）
rem   0.0.0.0    同网段的别的电脑也能打开；随机口令会打印在启动地址里，泄露即等于测试控制权泄露
set "UI_BIND=0.0.0.0"

echo 正在启动图形控制台...
echo 启动后程序会打印控制台地址（含一次性随机口令）；浏览器没自动弹出就复制那一行打开。
if /i not "%UI_BIND%"=="127.0.0.1" (
  echo 从别的电脑访问：把打印地址里的主机名换成本机测试网 IP，端口和 ?token= 照抄（只认 IP，别用域名）。
  echo 首次运行的防火墙提示请选择“允许访问”。
)
echo 保持此窗口打开；关掉它控制台就停了。
echo.
cpe_test.exe ui --ui-bind %UI_BIND% --token %AGENT_TOKEN%
echo.
echo 控制台已停止。
pause
