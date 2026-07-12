@echo off
setlocal
cd /d "%~dp0"

echo ========================================
echo          选择本次测试的网口配置
echo ========================================
echo [1] SGMII 1G / 2.5G
echo [2] Wi-Fi 5G
echo [3] 10G USB - 10G Ethernet
echo [4] 全部常用网口
echo.
set /p CHOICE=请输入编号（默认 1）：
if "%CHOICE%"=="" set "CHOICE=1"

if "%CHOICE%"=="1" set "CONFIG=configs\config-sgmii.json"
if "%CHOICE%"=="2" set "CONFIG=configs\config-wifi5g.json"
if "%CHOICE%"=="3" set "CONFIG=configs\config-10gusb.json"
if "%CHOICE%"=="4" set "CONFIG=configs\config-all-common.json"

if not defined CONFIG (
  echo [错误] 无效编号：%CHOICE%
  pause
  exit /b 1
)

call start_master.bat "%CONFIG%"
