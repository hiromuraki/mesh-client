# 复现 F2：core 已拉起但未就绪时，core 线程阻塞在就绪探测里最长 25s，
# 期间 Cmd::Stop 只能排队；Shutdown 只等 15s 就放弃并 process::exit。
# 做法：用 mesh-core 的 examples/fake_core（mode=no-portal：起进程但永不应答 rpc），
# 客户端起来后立刻 POST /api/quit，量退出耗时，并看那个 core 是否活到客户端退出之后。
# 只写临时安装根，不碰真实安装、不需要管理员权限。
$ErrorActionPreference = 'Stop'
$root = Join-Path $env:TEMP ("mesh-probe-quit-" + $PID)
$result = Join-Path $env:TEMP ("mesh-probe-quit-result-" + $PID + '.txt')
$client = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\target\debug\mesh-client.exe')).Path
$core = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..\target\debug\examples\fake_core.exe')).Path
Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$root\bin","$root\config","$root\log" | Out-Null
'{"version":"2026.11.27-1","files":{}}' | Set-Content "$root\bin\version.json"
$marker = "$root\core.pid"
$config = @{ server_url = 'http://127.0.0.1:9'; device_id = '0123456789abcdef0123456789abcdef'; hostname = 'probe'; core_path = $core } | ConvertTo-Json -Compress
$config | Set-Content "$root\config\client.json"
# 凭据节点没有凭据就不起 core：给一张假的（fake_core 只关心 ET_CREDENTIAL 在不在）。
'{"id":"cred-probe","secret":"c2VjcmV0"}' | Set-Content "$root\config\credential.json"
@('ipv4 = "10.99.99.99/24"', 'mode = "no-portal"', ('marker = "' + $marker.Replace('\','/') + '"')) | Set-Content "$root\config\config.toml"
$clientProc = Start-Process -FilePath $client -ArgumentList '--headless','--root',$root -PassThru -RedirectStandardOutput "$root\client.out.log" -RedirectStandardError "$root\client.err.log"
$lines = New-Object System.Collections.Generic.List[string]
try {
    $deadline = (Get-Date).AddSeconds(30)
    while (-not (Test-Path $marker) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 100 }
    if (-not (Test-Path $marker)) { throw ('core 没有起来: ' + (Get-Content "$root\log\client.log" -Raw)) }
    $corePid = [int]((Get-Content $marker -Raw).Trim())
    $lines.Add("core_pid=$corePid")
    $lines.Add("core_alive_before_quit=$([bool](Get-Process -Id $corePid -ErrorAction SilentlyContinue))")
    $ui = "$root\config\ui.json"
    $deadline = (Get-Date).AddSeconds(20)
    while (-not (Test-Path $ui) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 100 }
    $url = ((Get-Content $ui -Raw) | ConvertFrom-Json).url
    $lines.Add("panel=$url")
    $sw = [Diagnostics.Stopwatch]::StartNew()
    Invoke-WebRequest -Uri ($url + 'api/quit') -Method Post -UseBasicParsing -TimeoutSec 5 | Out-Null
    $lines.Add("quit_answered_ms=$($sw.ElapsedMilliseconds)")
    $clientProc.WaitForExit(60000) | Out-Null
    $sw.Stop()
    $lines.Add("exit_code=$($clientProc.ExitCode)")
    $lines.Add("client_exit_after_ms=$($sw.ElapsedMilliseconds)")
    Start-Sleep -Milliseconds 500
    $lines.Add("core_alive_after_client_exit=$([bool](Get-Process -Id $corePid -ErrorAction SilentlyContinue))")
    $lines.Add('--- client.log tail ---')
    (Get-Content "$root\log\client.log" -Tail 8) | ForEach-Object { $lines.Add($_) }
} catch {
    $lines.Add('ERROR: ' + $_.Exception.Message)
} finally {
    if ($corePid -and (Get-Process -Id $corePid -ErrorAction SilentlyContinue)) { Stop-Process -Id $corePid -Force -ErrorAction SilentlyContinue }
    if ($clientProc -and -not $clientProc.HasExited) { Stop-Process -Id $clientProc.Id -Force -ErrorAction SilentlyContinue }
    $lines | Set-Content $result
    Write-Output "result=$result"
    $lines | ForEach-Object { Write-Output $_ }
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}