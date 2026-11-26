# 复现 F1：engine::start_stun_loop 每次重连都新起一个永不退出的 STUN 线程。
# 起一个只回 204 的假控制面（stub-204.ps1），让每次 POST /api/reconnect 都能走完一轮
# connect（只有 connect 成功才会走到 start_stun_loop），然后数客户端进程的线程数。
# 只写临时安装根，不碰真实安装、不需要管理员权限。
$ErrorActionPreference = 'Stop'
$client = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\target\debug\mesh-client.exe')).Path
$stubScript = Join-Path $PSScriptRoot 'stub-204.ps1'
$root = Join-Path $env:TEMP ("mesh-probe-stun-" + $PID)
$result = Join-Path $env:TEMP ("mesh-probe-stun-result-" + $PID + '.txt')
Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$root\bin","$root\config","$root\log" | Out-Null
'{"version":"2026.11.27-1","files":{}}' | Set-Content "$root\bin\version.json"
$probe = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$probe.Start(); $port = $probe.LocalEndpoint.Port; $probe.Stop()
$stub = Start-Process -FilePath (Get-Process -Id $PID).Path -ArgumentList '-NoProfile','-File',('"' + $stubScript + '"'),'-Port',$port -PassThru -WindowStyle Hidden
$lines = New-Object System.Collections.Generic.List[string]
$clientProc = $null
try {
    Start-Sleep -Milliseconds 500
    $config = @{ server_url = "http://127.0.0.1:$port"; device_id = '0123456789abcdef0123456789abcdef'; hostname = 'probe' } | ConvertTo-Json -Compress
    $config | Set-Content "$root\config\client.json"
    'hostname = "probe"' + "`n" + 'ipv4 = "10.99.99.99/24"' | Set-Content "$root\config\config.toml"
    $clientProc = Start-Process -FilePath $client -ArgumentList '--headless','--root',$root -PassThru -RedirectStandardOutput "$root\client.out.log" -RedirectStandardError "$root\client.err.log"
    $ui = "$root\config\ui.json"
    $log = "$root\log\client.log"
    $deadline = (Get-Date).AddSeconds(30)
    while (-not (Test-Path $ui) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 200 }
    if (-not (Test-Path $ui)) { throw '面板没有起来' }
    $url = ((Get-Content $ui -Raw) | ConvertFrom-Json).url
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Date) -lt $deadline) {
        if ((Test-Path $log) -and ((Get-Content $log -Raw) -match '已连接服务端')) { break }
        Start-Sleep -Milliseconds 200
    }
    if (-not ((Get-Content $log -Raw) -match '已连接服务端')) { throw '首轮心跳没有完成' }
    Start-Sleep -Seconds 1
    $before = (Get-Process -Id $clientProc.Id).Threads.Count
    $lines.Add("panel=$url")
    $lines.Add("threads_before=$before")
    $n = 20
    $slow = @()
    for ($i = 1; $i -le $n; $i++) {
        $sw = [Diagnostics.Stopwatch]::StartNew()
        $code = (Invoke-WebRequest -Uri ($url + 'api/reconnect') -Method Post -UseBasicParsing -TimeoutSec 10).StatusCode
        $sw.Stop()
        if ($sw.ElapsedMilliseconds -gt 500) { $slow += "$i=$($sw.ElapsedMilliseconds)ms" }
        if ($code -ne 204) { $lines.Add("post_$i=$code") }
    }
    $lines.Add("reconnects=$n slow_posts=$($slow -join ',')")
    Start-Sleep -Seconds 5
    $after = (Get-Process -Id $clientProc.Id).Threads.Count
    $lines.Add("threads_after=$after")
    $lines.Add("threads_delta=$($after - $before)")
    $lines.Add('--- client.log tail ---')
    (Get-Content $log -Tail 12) | ForEach-Object { $lines.Add($_) }
} catch {
    $lines.Add('ERROR: ' + $_.Exception.Message)
} finally {
    if ($clientProc -and -not $clientProc.HasExited) {
        try { Invoke-WebRequest -Uri ("http://127.0.0.1:" + ([uri]$url).Port + '/api/quit') -Method Post -UseBasicParsing -TimeoutSec 5 | Out-Null } catch { }
        Start-Sleep -Seconds 2
        if (-not $clientProc.HasExited) { Stop-Process -Id $clientProc.Id -Force -ErrorAction SilentlyContinue }
    }
    Stop-Process -Id $stub.Id -Force -ErrorAction SilentlyContinue
    $lines | Set-Content $result
    Write-Output "result=$result"
    $lines | ForEach-Object { Write-Output $_ }
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}