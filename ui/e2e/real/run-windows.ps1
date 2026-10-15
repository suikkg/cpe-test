<#
真机验收（Windows）：起一个不带 --config 的控制台（默认判定口径），临时放通测试端口，
跑 e2e/real 下的 Playwright 用例，最后核对本机没有残留。CI 不跑。

用法（在 ui\ 目录、管理员 PowerShell 里；需要先 npm ci 和 npx playwright install chromium）：
  $env:CPE_REAL_SUBNET_A  = '192.168.8.103'   # 两块本机网卡，跑子网快捷测试；不填就跳过
  $env:CPE_REAL_SUBNET_B  = '192.168.8.108'
  $env:CPE_REAL_INNER_NIC = '192.168.8.103'   # 接到 CPE 的网卡，跑内环；不填就跳过
  $env:CPE_REAL_BOARD_IP  = '192.168.8.1'
  $env:CPE_REAL_ADB       = 'C:\platform-tools\adb.exe'
  .\e2e\real\run-windows.ps1 -Exe ..\target\release\cpe_test.exe

证据（请求、状态、报告、Excel、下载包、网卡计数、收尾核对）写到
test-results\real-evidence\<时间>\。板侧残留由用例自己经 ADB 核对。

本文件带 UTF-8 BOM：Windows PowerShell 5.1 把无 BOM 的脚本按系统代码页（GBK）读，
中文字符串会把解析弄坏。编辑后保持 BOM。
#>
param(
  [Parameter(Mandatory = $true)] [string] $Exe,
  [int] $Port = 29889
)
$ErrorActionPreference = 'Stop'
$OutputEncoding = [Console]::OutputEncoding = [Text.Encoding]::UTF8
$ui = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$exePath = (Resolve-Path $Exe).Path
$token = [guid]::NewGuid().ToString('N')
$env:CPE_UI_TOKEN = $token
$env:CPE_REAL_URL = "http://127.0.0.1:$Port"
$evidence = Join-Path $ui ('test-results\real-evidence\' + (Get-Date -Format 'yyyyMMdd_HHmmss'))
New-Item -ItemType Directory -Force $evidence | Out-Null
$env:CPE_REAL_EVIDENCE = $evidence
$rules = @('TCP', 'UDP', 'ICMPv4') | ForEach-Object { "CPE-real-acceptance-$_" }
$headers = @{ 'X-CPE-Token' = $token; 'X-CPE-Console' = '1' }
$server = $null
$result = 1
try {
  # 测试端口从 56000 起分配（子网到 65535 回绕，内环从 56190 起）。只对本网段放通。
  New-NetFirewallRule -DisplayName $rules[0] -Direction Inbound -Action Allow -Protocol TCP -LocalPort 56000-65535 -RemoteAddress LocalSubnet | Out-Null
  New-NetFirewallRule -DisplayName $rules[1] -Direction Inbound -Action Allow -Protocol UDP -LocalPort 56000-65535 -RemoteAddress LocalSubnet | Out-Null
  New-NetFirewallRule -DisplayName $rules[2] -Direction Inbound -Action Allow -Protocol ICMPv4 -RemoteAddress LocalSubnet | Out-Null
  Get-NetAdapterStatistics | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $evidence 'nic-before.json') -Encoding UTF8

  # 控制台的 runs\ 与 inner_runs\ 落在证据目录里，不混进别的历史。
  $work = Join-Path $evidence 'console'
  New-Item -ItemType Directory -Force $work | Out-Null
  $server = Start-Process $exePath -ArgumentList @('ui', '--port', "$Port") -WorkingDirectory $work `
    -RedirectStandardOutput (Join-Path $evidence 'console.log') -RedirectStandardError (Join-Path $evidence 'console.err.log') -PassThru
  # 等控制台真的能应答，不睡固定秒数。
  $deadline = (Get-Date).AddSeconds(60)
  while ($true) {
    try { Invoke-RestMethod -Uri "$env:CPE_REAL_URL/api/bootstrap" -Headers $headers -TimeoutSec 2 | Out-Null; break } catch {
      if ($server.HasExited) { throw "控制台已退出，见 $evidence\console.err.log" }
      if ((Get-Date) -gt $deadline) { throw '控制台 60 秒内没有应答' }
      Start-Sleep -Milliseconds 500
    }
  }
  Push-Location $ui
  try {
    & npx playwright test -c playwright.real.config.ts --reporter=line
    $result = $LASTEXITCODE
  } finally { Pop-Location }
} finally {
  if ($server -and -not $server.HasExited) {
    foreach ($path in @('/api/stop', '/api/inner/stop')) {
      try { Invoke-RestMethod -Method Post -Uri "$env:CPE_REAL_URL$path" -Headers $headers -ContentType 'application/json' -Body '{}' | Out-Null } catch {}
    }
    Start-Sleep -Seconds 3
    Stop-Process -Id $server.Id -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2
  }
  Get-NetAdapterStatistics | ConvertTo-Json -Depth 4 | Set-Content (Join-Path $evidence 'nic-after.json') -Encoding UTF8
  foreach ($name in $rules) { Get-NetFirewallRule -DisplayName $name -ErrorAction SilentlyContinue | Remove-NetFirewallRule }
  # 进程与规则是唯一可靠的残留证据：本机临时端口段和测试端口段重叠，按端口查监听会误报别的程序。
  $left = [ordered]@{
    console = @(if ($server) { Get-Process -Id $server.Id -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id })
    iperf3 = @(Get-Process iperf3 -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id)
    firewall_rules = @(Get-NetFirewallRule -ErrorAction SilentlyContinue | Where-Object DisplayName -like 'CPE-real-acceptance-*' | Select-Object -ExpandProperty DisplayName)
    console_port = @(Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue | Select-Object -ExpandProperty OwningProcess)
  }
  $left | ConvertTo-Json | Set-Content (Join-Path $evidence 'cleanup.json') -Encoding UTF8
  if (@($left.Values | Where-Object { $_.Count -gt 0 }).Count -gt 0) {
    Write-Output "收尾核对未通过，见 $evidence\cleanup.json"
    if ($result -eq 0) { $result = 1 }
  }
  Write-Output "证据目录：$evidence"
}
exit $result
