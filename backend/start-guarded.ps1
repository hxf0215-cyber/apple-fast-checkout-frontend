$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root
New-Item -ItemType Directory -Force -Path (Join-Path $root "logs") | Out-Null
while ($true) {
    $started = Get-Date
    & "$root\target\release\apple-fast-checkout.exe"
    $exitCode = $LASTEXITCODE
    $message = "服务退出：$exitCode，时间：$(Get-Date -Format o)"
    Add-Content -Path "$root\logs\service-guard.log" -Value $message
    Start-Sleep -Seconds 3
}
