# 检查当前运行的服务版本
# 用法: .\check-version.ps1 [-Port 8080]

param([int]$Port = 8080)

$ErrorActionPreference = "SilentlyContinue"

Write-Host "==========================================" -ForegroundColor White
Write-Host " Apple Fast Checkout - 版本检查" -ForegroundColor White
Write-Host "==========================================" -ForegroundColor White

# 1) 服务是否在跑
$proc = Get-Process AppleFastCheckout, apple-fast-checkout -ErrorAction SilentlyContinue | Select-Object -First 1
if ($proc) {
    Write-Host "[进程] 运行中 (PID $($proc.Id))" -ForegroundColor Green
    Write-Host "       路径: $($proc.Path)" -ForegroundColor Gray
    $exeTime = (Get-Item $proc.Path).LastWriteTime
    Write-Host "       编译时间: $exeTime" -ForegroundColor Gray
} else {
    Write-Host "[进程] 未运行" -ForegroundColor Yellow
}

# 2) 探测端口
$found = $null
foreach ($p in $Port..($Port + 9)) {
    try {
        $r = Invoke-RestMethod -Uri "http://127.0.0.1:$p/api/version" -TimeoutSec 2
        $found = @{ Port = $p; Data = $r }
        break
    } catch {}
}

if (-not $found) {
    Write-Host ""
    Write-Host "[结果] 未找到运行中的服务（尝试端口 $Port-$($Port+9)）" -ForegroundColor Red
    exit 1
}

Write-Host ""
Write-Host "[服务] 端口 $($found.Port)" -ForegroundColor Green
Write-Host "       版本: $($found.Data.version)" -ForegroundColor Cyan
Write-Host "       构建: $($found.Data.build)" -ForegroundColor Cyan
Write-Host "       Git : $($found.Data.git)" -ForegroundColor Cyan

# 3) 前端产物是否与该后端匹配（用 index.html 的入口 hash 判断）
try {
    $html = (Invoke-WebRequest -Uri "http://127.0.0.1:$($found.Port)/static/web/index.html" -UseBasicParsing -TimeoutSec 3).Content
    $m = [regex]::Match($html, "index-([A-Za-z0-9_-]+)\.js")
    if ($m.Success) {
        Write-Host "       前端入口: index-$($m.Groups[1].Value).js" -ForegroundColor Cyan
    }
} catch {}

Write-Host "==========================================" -ForegroundColor White

exit 0
