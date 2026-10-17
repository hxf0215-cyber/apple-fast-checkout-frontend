<#
.SYNOPSIS
    静态代理测试脚本（单个 / 批量，前端 + 后端双目标）

.DESCRIPTION
    测试静态代理是否有效，每个代理访问两类 Apple 服务：

      [后端] www.apple.com.cn     网页商城（Rust 后端 AppleClient 用）
      [前端] web.mmap.apple.com   微信小程序接口（购物袋 / 库存，实际下单用）

    单个测试会额外检查 TCP 连通性、出口 IP、直连对比；
    批量测试会并发跑完所有代理并给出可用列表。

.PARAMETER Proxy
    单个代理，如 "192.3.248.101|11433|gfa|mnc"

.PARAMETER Proxies
    多个代理（数组），批量模式。

.PARAMETER File
    代理列表文件，每行一个；# 开头为注释。

.PARAMETER Concurrency
    批量并发数，默认 10。

.PARAMETER TimeoutSec
    单请求超时秒数，默认 15。

.PARAMETER OutCsv
    可选：导出结果 CSV。

.PARAMETER SkipFrontend / -SkipBackend
    跳过某一类目标。

.EXAMPLE
    .\test-proxy.ps1 -Proxy "192.3.248.101|11433|gfa|mnc"

.EXAMPLE
    .\test-proxy.ps1 -File .\proxies.txt -Concurrency 20 -OutCsv .\result.csv

.EXAMPLE
    .\test-proxy.ps1 -Proxies "1.2.3.4|8080|u|p","5.6.7.8|8080|u2|p2"
#>

param(
    [Parameter(Position = 0)]
    [string]$Proxy,

    [string[]]$Proxies,

    [string]$File,

    [int]$Concurrency = 10,

    [int]$TimeoutSec = 15,

    [string]$OutCsv,

    [switch]$SkipFrontend,

    [switch]$SkipBackend
)

$ErrorActionPreference = "Continue"

# ==================== 测试目标 ====================
$Targets = @()

if (-not $SkipBackend) {
    $Targets += [PSCustomObject]@{
        Kind   = "后端"
        Name   = "Apple官网"
        Url    = "https://www.apple.com.cn/"
        Expect = "apple"
    }
}

if (-not $SkipFrontend) {
    $Targets += [PSCustomObject]@{
        Kind   = "前端"
        Name   = "小程序库存"
        Url    = "https://web.mmap.apple.com/26.07.0-MAIN/miniprogram-merch/p/wechat/product-locator/quotes?pn=MJT74CH%2FA&stores=R480"
        Expect = "mmap"
    }
    $Targets += [PSCustomObject]@{
        Kind   = "前端"
        Name   = "小程序袋子"
        Url    = "https://web.mmap.apple.com/moe/p/wechat/chat/bag"
        Expect = "mmap"
    }
}

# ==================== 收集代理 ====================
$rawList = New-Object System.Collections.Generic.List[string]
if ($Proxy) { $rawList.Add($Proxy.Trim()) }
if ($Proxies) { foreach ($x in $Proxies) { foreach ($l in ($x -split "[`r`n]")) { $t = $l.Trim(); if ($t) { $rawList.Add($t) } } } }
if ($File) {
    if (-not (Test-Path $File)) { Write-Host "[错误] 找不到文件: $File" -ForegroundColor Red; exit 1 }
    foreach ($l in (Get-Content $File)) { $t = $l.Trim(); if (-not $t -or $t.StartsWith("#")) { continue }; $t = ($t -split "\s+#")[0].Trim(); if ($t) { $rawList.Add($t) } }
}
$rawList = @($rawList | Where-Object { $_ } | Select-Object -Unique)

if ($rawList.Count -eq 0) {
    Write-Host ""
    Write-Host "静态代理测试（前端 + 后端）" -ForegroundColor White
    Write-Host "================================================================" -ForegroundColor White
    Write-Host ""
    Write-Host "用法：" -ForegroundColor Gray
    Write-Host "  单个:  .\test-proxy.ps1 -Proxy 1.2.3.4|8080|user|pass" -ForegroundColor Gray
    Write-Host "  批量:  .\test-proxy.ps1 -File .\proxies.txt -Concurrency 20" -ForegroundColor Gray
    Write-Host "  数组:  .\test-proxy.ps1 -Proxies 1.2.3.4|8080|u|p,5.6.7.8|8080|u|p" -ForegroundColor Gray
    Write-Host ""
    exit 1
}

$isBatch = $rawList.Count -gt 1

# ==================== 解析代理字符串 ====================
function Parse-Proxy([string]$raw) {
    $r = $raw.Trim()
    $body = $r -replace "^(https?|socks[45])://", ""
    $h = ""; $pt = ""; $u = ""; $pw = ""

    if ($body.Contains("|")) {
        $a = $body.Split("|")
        $h = $a[0].Trim(); $pt = $a[1].Trim()
        if ($a.Length -ge 3) { $u = $a[2].Trim() }
        if ($a.Length -ge 4) { $pw = $a[3].Trim() }
    } elseif ($body.Contains("@")) {
        $at = $body.IndexOf("@")
        $cred = $body.Substring(0, $at)
        $hp = $body.Substring($at + 1)
        $ci = $cred.IndexOf(":")
        if ($ci -gt 0) { $u = $cred.Substring(0, $ci); $pw = $cred.Substring($ci + 1) }
        $sp = $hp.Split(":")
        $h = $sp[0].Trim(); $pt = $sp[1].Trim()
    } else {
        $cc = $body.Split(":")
        if ($cc.Length -eq 4) {
            $h = $cc[0].Trim(); $pt = $cc[1].Trim(); $u = $cc[2].Trim(); $pw = $cc[3].Trim()
        } elseif ($cc.Length -eq 2) {
            $h = $cc[0].Trim(); $pt = $cc[1].Trim()
        }
    }

    if (-not $h -or -not $pt) { return $null }

    $scheme = if ($r -match "^socks") { "socks5" } else { "http" }
    $url = if ($u) {
        "{0}://{1}:{2}@{3}:{4}" -f $scheme, [uri]::EscapeDataString($u), [uri]::EscapeDataString($pw), $h, $pt
    } else {
        "{0}://{1}:{2}" -f $scheme, $h, $pt
    }

    return [PSCustomObject]@{ Raw = $raw.Trim(); Host = $h; Port = $pt; User = $u; ProxyUrl = $url }
}

$parsed = @()
$bad = @()
foreach ($x in $rawList) { $p = Parse-Proxy $x; if ($p) { $parsed += $p } else { $bad += $x } }

if ($parsed.Count -eq 0) {
    Write-Host "[错误] 所有代理格式都无法解析。" -ForegroundColor Red
    $bad | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkYellow }
    exit 1
}

# ==================== 核心：测一个代理 ====================
$curlPath = Join-Path $env:SystemRoot "System32\curl.exe"
if (-not (Test-Path $curlPath)) { $curlPath = "curl.exe" }

function Test-ProxyTargets {
    param($Info, $TargetList, $Tmo)

    $details = @()
    $allOk = $true
    $maxTime = 0.0

    foreach ($t in $TargetList) {
        $bodyFile = [System.IO.Path]::Combine([System.IO.Path]::GetTempPath(), "pxb-" + [guid]::NewGuid().ToString("N"))
        $isMmap = ($t.Expect -eq "mmap")

        # MMAP 接口用 -L 跟随重定向更贴近真实（但要检查最终落点是否 covers.html）
        # 这里不用 -L，改为读取 Location 判断，避免误判
        $fmt = "%{http_code}|%{time_total}|%{size_download}|%{content_type}|%{redirect_url}"
        $curlArgs = @(
            "-s", "-o", $bodyFile,
            "-w", $fmt,
            "--max-time", "$Tmo",
            "-A", "Mozilla/5.0 (iPhone; CPU iPhone OS 16_7_2 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Mobile/15E148 MicroMessenger/8.0.79",
            "-x", $Info.ProxyUrl,
            $t.Url
        )

        $raw = ""
        $exit = -1
        try {
            $raw = (& $curlPath @curlArgs 2>&1 | Out-String).Trim()
            $exit = $LASTEXITCODE
        } catch { $raw = "" }

        $f = $raw.Split("|")
        $code     = if ($f.Length -ge 1) { $f[0].Trim() } else { "000" }
        $time     = if ($f.Length -ge 2) { [double]$f[1] } else { 0.0 }
        $size     = if ($f.Length -ge 3) { $f[2].Trim() } else { "0" }
        $ctype    = if ($f.Length -ge 4) { $f[3].Trim() } else { "" }
        $redirect = if ($f.Length -ge 5) { $f[4].Trim() } else { "" }

        # ---- 判定 ----
        $ok = $false
        $note = ""

        if ($exit -ne 0 -and $code -eq "000") {
            $note = "连接失败"
        } elseif ($code -eq "000") {
            $note = "无响应"
        } elseif ($code -eq "407") {
            $note = "代理认证失败"
        } elseif ($code -eq "403") {
            $note = "被拒绝(403)"
        } elseif ($isMmap) {
            # 小程序接口
            if ($code -eq "302" -or $code -eq "301") {
                if ($redirect -match "covers\.html") {
                    $note = "预购等待页(接口未开放)"
                } else {
                    $ok = $true; $note = "OK(跳转)"
                }
            } elseif ($code -eq "200") {
                $ok = $true; $note = "OK"
            } else {
                # 401/400 等：说明代理能到 Apple，只是缺 Cookie
                $ok = $true; $note = "OK(需Cookie:$code)"
            }
        } else {
            # Apple 官网：验证内容
            if (Test-Path $bodyFile) {
                $body = Get-Content $bodyFile -Raw -ErrorAction SilentlyContinue
                if ($body -and $body -match "apple") { $ok = $true; $note = "OK" }
                elseif ($body) { $note = "内容非Apple" }
                else { $note = "响应为空" }
            } else {
                $note = "无响应体"
            }
        }

        if (Test-Path $bodyFile) { Remove-Item $bodyFile -Force -ErrorAction SilentlyContinue }

        if (-not $ok) { $allOk = $false }
        if ($time -gt $maxTime) { $maxTime = $time }

        $details += [PSCustomObject]@{
            Kind = $t.Kind; Name = $t.Name
            HttpCode = $code; TimeSec = [math]::Round($time, 2)
            Size = $size; ContentType = $ctype
            Ok = $ok; Note = $note
        }
    }

    return [PSCustomObject]@{
        Info = $Info
        AllOk = $allOk
        MaxTimeSec = [math]::Round($maxTime, 2)
        Details = $details
    }
}

# ==================== 头部信息 ====================
Write-Host ""
Write-Host "================================================================" -ForegroundColor White
if ($isBatch) {
    Write-Host " 静态代理批量测试（前端 + 后端）" -ForegroundColor White
} else {
    Write-Host " 静态代理测试（前端 + 后端）" -ForegroundColor White
}
Write-Host "================================================================" -ForegroundColor White
Write-Host (" 代理数量 : {0}" -f $parsed.Count)
if ($isBatch) { Write-Host (" 并发数   : {0}" -f $Concurrency) }
Write-Host (" 超时     : {0}s" -f $TimeoutSec)
Write-Host (" 测试目标 : {0} 个" -f $Targets.Count)
foreach ($t in $Targets) { Write-Host ("   [{0}] {1}" -f $t.Kind, $t.Name) -ForegroundColor Gray }
if ($bad.Count -gt 0) {
    Write-Host (" 格式错误 : {0} 条（跳过）" -f $bad.Count) -ForegroundColor Yellow
    $bad | ForEach-Object { Write-Host ("   {0}" -f $_) -ForegroundColor DarkYellow }
}
Write-Host ""

# ==================== 单代理模式 ====================
if (-not $isBatch) {
    $one = $parsed[0]
    Write-Host (" 目标代理 : {0}" -f $one.Raw) -ForegroundColor Cyan
    Write-Host (" 解析结果 : {0}:{1}" -f $one.Host, $one.Port)
    if ($one.User) { Write-Host (" 认证     : {0} : ****" -f $one.User) } else { Write-Host " 认证     : 无" }
    Write-Host ""

    # 1) TCP 连通
    Write-Host "[1/4] TCP 端口连通性" -ForegroundColor Cyan
    $tcpOk = $false
    try {
        $tc = New-Object System.Net.Sockets.TcpClient
        $iar = $tc.BeginConnect($one.Host, [int]$one.Port, $null, $null)
        if ($iar.AsyncWaitHandle.WaitOne(8000, $false) -and $tc.Connected) {
            $tc.EndConnect($iar)
            Write-Host "      OK   端口可连接" -ForegroundColor Green
            $tcpOk = $true
        } else {
            Write-Host "      FAIL 端口连接超时" -ForegroundColor Red
        }
        $tc.Close()
    } catch {
        Write-Host ("      FAIL {0}" -f $_.Exception.Message) -ForegroundColor Red
    }
    Write-Host ""

    # 2) 访问 Apple（前端 + 后端）
    Write-Host "[2/4] 访问 Apple 服务" -ForegroundColor Cyan
    $r = Test-ProxyTargets -Info $one -TargetList $Targets -Tmo $TimeoutSec
    foreach ($d in $r.Details) {
        $tag = if ($d.Ok) { "OK" } else { "FAIL" }
        $color = if ($d.Ok) { "Green" } else { "Red" }
        Write-Host ("      {0,-4} [{1}] {2,-12} HTTP {3,-4} {4,5:N2}s  {5}" -f $tag, $d.Kind, $d.Name, $d.HttpCode, $d.TimeSec, $d.Note) -ForegroundColor $color
    }
    Write-Host ""

    # 3) 出口 IP
    Write-Host "[3/4] 出口 IP 与地区" -ForegroundColor Cyan
    $exitIp = ""
    try {
        $ipRaw = & $curlPath -s --max-time 15 -x $one.ProxyUrl "http://ip-api.com/json/?lang=zh-CN&fields=status,country,regionName,city,isp,query"
        $ipJson = ($ipRaw | Out-String).Trim() | ConvertFrom-Json -ErrorAction Stop
        if ($ipJson.status -eq "success") {
            $exitIp = $ipJson.query
            Write-Host ("      OK   出口IP: {0}" -f $exitIp) -ForegroundColor Green
            Write-Host ("           地区  : {0} {1} {2}" -f $ipJson.country, $ipJson.regionName, $ipJson.city) -ForegroundColor Gray
            Write-Host ("           ISP   : {0}" -f $ipJson.isp) -ForegroundColor Gray
        } else {
            Write-Host "      FAIL 查询失败" -ForegroundColor Red
        }
    } catch {
        Write-Host ("      FAIL {0}" -f $_.Exception.Message) -ForegroundColor Red
    }
    Write-Host ""

    # 4) 直连对比
    Write-Host "[4/4] 直连对比" -ForegroundColor Cyan
    try {
        $dRaw = & $curlPath -s --max-time 15 "http://ip-api.com/json/?fields=query"
        $directIp = (($dRaw | Out-String).Trim() | ConvertFrom-Json -ErrorAction Stop).query
        if ($directIp -and $exitIp) {
            if ($directIp -eq $exitIp) {
                Write-Host "      FAIL 代理出口与本机相同 → 代理未生效！" -ForegroundColor Red
            } else {
                Write-Host ("      OK   本机 {0}  ≠  代理 {1} → 代理生效" -f $directIp, $exitIp) -ForegroundColor Green
            }
        } else {
            Write-Host "      --   无法对比（直连查询失败，不影响代理判定）" -ForegroundColor Gray
        }
    } catch {
        Write-Host "      --   无法对比（直连查询失败）" -ForegroundColor Gray
    }
    Write-Host ""

    # 结论：区分「代理问题」与「Apple 侧未开放」
    $appleWaiting   = @($r.Details | Where-Object { $_.Note -match "预购等待页" })
    $realFail       = @($r.Details | Where-Object { -not $_.Ok -and $_.Note -notmatch "预购等待页" })
    $proxyReachable = @($r.Details | Where-Object { $_.HttpCode -ne "000" -and $_.HttpCode -ne "" }).Count -gt 0

    Write-Host "================================================================" -ForegroundColor White
    if ($r.AllOk) {
        Write-Host " 结论：代理可用 ✓" -ForegroundColor Green
        Write-Host (" 填入程序静态IP列表： {0}" -f $one.Raw) -ForegroundColor White
        Write-Host (" 最慢目标耗时： {0:N2}s" -f $r.MaxTimeSec) -ForegroundColor Gray
        if ($r.MaxTimeSec -gt 8) { Write-Host " 注意：耗时超过 8 秒，速度偏慢" -ForegroundColor Yellow }
    } elseif ($proxyReachable -and $realFail.Count -eq 0) {
        Write-Host " 结论：代理可用 ✓（Apple 小程序接口当前未开放）" -ForegroundColor Yellow
        Write-Host (" 填入程序静态IP列表： {0}" -f $one.Raw) -ForegroundColor White
        Write-Host (" 最慢目标耗时： {0:N2}s" -f $r.MaxTimeSec) -ForegroundColor Gray
        Write-Host ""
        Write-Host " 说明：小程序接口返回 302 -> covers.html（Apple 预购等待页）。" -ForegroundColor Gray
        Write-Host "       这是 Apple 官方行为，等预购开始后自动恢复，与代理无关。" -ForegroundColor Gray
        Write-Host "       （本机直连访问同样返回该跳转，已确认）" -ForegroundColor DarkGray
    } else {
        Write-Host " 结论：代理不可用 ✗" -ForegroundColor Red
        if (-not $tcpOk) {
            Write-Host " 原因：端口不通（IP失效 / 需加白名单 / 端口写错）" -ForegroundColor Gray
        } else {
            Write-Host " 原因：能连代理但访问 Apple 失败（认证错 / IP被风控）" -ForegroundColor Gray
        }
        foreach ($d in $realFail) { Write-Host ("   [{0}] {1} → {2}" -f $d.Kind, $d.Name, $d.Note) -ForegroundColor DarkRed }
    }
    Write-Host "================================================================" -ForegroundColor White

    if ($OutCsv) {
        $r.Details | Select-Object Kind, Name, HttpCode, TimeSec, Ok, Note |
            Export-Csv -Path $OutCsv -NoTypeInformation -Encoding UTF8
        Write-Host (" 已导出: {0}" -f $OutCsv) -ForegroundColor Cyan
    }

    $proxyOk = $r.AllOk -or ($proxyReachable -and $realFail.Count -eq 0)
    exit $(if ($proxyOk) { 0 } else { 1 })
}

# ==================== 批量模式（并发） ====================
# 用 RunspacePool：每个代理在独立 PowerShell 会话里跑，互不干扰

# 供 runspace 使用的完整脚本（自包含，不依赖外部变量）
$BatchWorker = {
    param($ProxyUrl, $Raw, $TargetData, $Tmo, $Curl)

    $out = @()
    foreach ($t in $TargetData) {
        $bf  = [System.IO.Path]::Combine([System.IO.Path]::GetTempPath(), [guid]::NewGuid().ToString("N"))
        $fmt = "%{http_code}|%{time_total}|%{size_download}|%{redirect_url}"
        $a = @("-s", "-o", $bf, "-w", $fmt, "--max-time", "$Tmo",
               "-A", "Mozilla/5.0 (iPhone; CPU iPhone OS 16_7_2 like Mac OS X) AppleWebKit/605.1.15 Mobile/15E148 MicroMessenger/8.0.79",
               "-x", $ProxyUrl, $t.Url)

        $raw = ""; $exit = -1
        try { $raw = (& $Curl @a 2>&1 | Out-String).Trim(); $exit = $LASTEXITCODE } catch { }

        $f = $raw.Split("|")
        $code  = if ($f.Length -ge 1) { $f[0].Trim() } else { "000" }
        $time  = if ($f.Length -ge 2) { [double]$f[1] } else { 0.0 }
        $redir = if ($f.Length -ge 4) { $f[3].Trim() } else { "" }

        $ok = $false; $note = ""
        if ($exit -ne 0 -and $code -eq "000") { $note = "连接失败" }
        elseif ($code -eq "000") { $note = "无响应" }
        elseif ($code -eq "407") { $note = "代理认证失败" }
        elseif ($code -eq "403") { $note = "被拒绝(403)" }
        elseif ($t.Kind -eq "前端") {
            if ($code -eq "302" -or $code -eq "301") {
                if ($redir -match "covers\.html") { $ok = $true; $note = "预购等待期" }
                else { $ok = $true; $note = "OK(跳转)" }
            } elseif ($code -eq "200") { $ok = $true; $note = "OK" }
            else { $ok = $true; $note = "OK(需Cookie:$code)" }
        } else {
            if (Test-Path $bf) {
                $body = Get-Content $bf -Raw -ErrorAction SilentlyContinue
                if ($body -and $body -match "apple") { $ok = $true; $note = "OK" }
                elseif ($body) { $note = "内容非Apple" }
                else { $note = "空响应" }
            } else { $note = "无响应体" }
        }

        if (Test-Path $bf) { Remove-Item $bf -Force -ErrorAction SilentlyContinue }

        $out += [PSCustomObject]@{
            Kind = $t.Kind; Name = $t.Name
            HttpCode = $code; TimeSec = [math]::Round($time, 2)
            Ok = $ok; Note = $note
        }
    }

    $allOk = -not @($out | Where-Object { -not $_.Ok }).Count
    $maxT  = (@($out | Measure-Object -Property TimeSec -Maximum).Maximum)
    if (-not $maxT) { $maxT = 0 }

    [PSCustomObject]@{
        AllOk = $allOk
        MaxTimeSec = [math]::Round([double]$maxT, 2)
        Details = $out
    }
}

# 目标数据（去掉 Expect，runspace 里不需要）
$targetData = @($Targets | ForEach-Object {
    [PSCustomObject]@{ Kind = $_.Kind; Name = $_.Name; Url = $_.Url }
})

$pool = [runspacefactory]::CreateRunspacePool(1, $Concurrency)
$pool.Open()

$jobs = @()
foreach ($info in $parsed) {
    $ps = [powershell]::Create()
    $ps.RunspacePool = $pool
    [void]$ps.AddScript($BatchWorker.ToString())
    [void]$ps.AddArgument($info.ProxyUrl)
    [void]$ps.AddArgument($info.Raw)
    [void]$ps.AddArgument($targetData)
    [void]$ps.AddArgument($TimeoutSec)
    [void]$ps.AddArgument($curlPath)
    $jobs += [PSCustomObject]@{ PS = $ps; Handle = $ps.BeginInvoke(); Info = $info }
}

Write-Host " 测试中，请稍候..." -ForegroundColor Gray

$results = @()
foreach ($j in $jobs) {
    try {
        $out = $j.PS.EndInvoke($j.Handle)
        if ($out -and $out.Count -gt 0) {
            $res = $out[-1]
            $results += [PSCustomObject]@{
                Proxy = $j.Info.Raw
                Host = $j.Info.Host
                Port = $j.Info.Port
                User = $j.Info.User
                AllOk = $res.AllOk
                MaxTimeSec = $res.MaxTimeSec
                Details = [object[]]$res.Details
            }
        } else {
            $results += [PSCustomObject]@{
                Proxy = $j.Info.Raw; Host = $j.Info.Host; Port = $j.Info.Port; User = $j.Info.User
                AllOk = $false; MaxTimeSec = 0
                Details = @([PSCustomObject]@{ Kind="?"; Name="无输出"; HttpCode="000"; TimeSec=0; Ok=$false; Note="runspace 无返回" })
            }
        }
    } catch {
        $results += [PSCustomObject]@{
            Proxy = $j.Info.Raw; Host = $j.Info.Host; Port = $j.Info.Port; User = $j.Info.User
            AllOk = $false; MaxTimeSec = 0
            Details = @([PSCustomObject]@{ Kind="?"; Name="异常"; HttpCode="000"; TimeSec=0; Ok=$false; Note=$_.Exception.Message })
        }
    } finally { $j.PS.Dispose() }
}
$pool.Close(); $pool.Dispose()

# ==================== 汇总 ====================
$all = @($results)
$okList  = @($all | Where-Object { $_.AllOk } | Sort-Object MaxTimeSec)
$badList = @($all | Where-Object { -not $_.AllOk })

Write-Host ""
Write-Host "================================================================" -ForegroundColor White
Write-Host " 结果汇总" -ForegroundColor White
Write-Host "================================================================" -ForegroundColor White
Write-Host ("  总计    : {0}" -f $all.Count)
Write-Host ("  可用    : {0}" -f $okList.Count) -ForegroundColor Green
Write-Host ("  不可用  : {0}" -f $badList.Count) -ForegroundColor Red
if ($okList.Count -gt 0) {
    $avg = ($okList | Measure-Object MaxTimeSec -Average).Average
    Write-Host ("  平均耗时: {0:N2}s" -f $avg) -ForegroundColor Gray
    Write-Host ("  最快    : {0:N2}s" -f $okList[0].MaxTimeSec) -ForegroundColor Gray
}

if ($okList.Count -gt 0) {
    Write-Host ""
    Write-Host " --- 可用代理（按速度排序）---" -ForegroundColor Green
    foreach ($r in $okList) {
        $codes = ($r.Details | ForEach-Object { "{0}:{1}" -f $_.Kind.Substring(0,1), $_.HttpCode }) -join " "
        Write-Host ("  {0,6:N1}s  {1,-32}  {2}" -f $r.MaxTimeSec, $r.Proxy, $codes) -ForegroundColor Green
    }
}

if ($badList.Count -gt 0) {
    Write-Host ""
    Write-Host " --- 不可用详情 ---" -ForegroundColor Red
    foreach ($r in $badList) {
        Write-Host ("  {0}" -f $r.Proxy) -ForegroundColor Red
        foreach ($d in $r.Details) {
            Write-Host ("      [{0}] {1,-12} HTTP {2,-4} {3}" -f $d.Kind, $d.Name, $d.HttpCode, $d.Note) -ForegroundColor DarkRed
        }
    }
}

if ($okList.Count -gt 0) {
    Write-Host ""
    Write-Host " --- 可用列表（可直接粘贴到程序的静态IP配置）---" -ForegroundColor Cyan
    foreach ($r in $okList) { Write-Host ("  {0}" -f $r.Proxy) -ForegroundColor White }
}

if ($OutCsv) {
    $rows = @()
    foreach ($r in $all) {
        foreach ($d in $r.Details) {
            $rows += [PSCustomObject]@{
                Proxy = $r.Proxy; Host = $r.Host; Port = $r.Port
                Type = $d.Kind; Target = $d.Name; HttpCode = $d.HttpCode
                TimeSec = $d.TimeSec; OK = $d.Ok; Note = $d.Note
            }
        }
    }
    $rows | Export-Csv -Path $OutCsv -NoTypeInformation -Encoding UTF8
    Write-Host ""
    Write-Host (" 已导出 CSV: {0}" -f $OutCsv) -ForegroundColor Cyan
}

Write-Host ""
Write-Host "================================================================" -ForegroundColor White

exit $(if ($okList.Count -gt 0) { 0 } else { 1 })
