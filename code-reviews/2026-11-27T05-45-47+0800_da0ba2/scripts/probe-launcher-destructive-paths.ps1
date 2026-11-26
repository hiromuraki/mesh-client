# 复现 F3 / F4：启动器的两条破坏性行为。
#   A) `mesh --file <包>` 的摘要不符时，把用户给的包文件删掉；
#   B) `mesh --file` 安装前不 recover：上次中断留下的 bin.old（唯一好副本）被直接删掉，
#      这次安装再失败时 restore 把坏的 bin 换回去，安装根变成“没有好版本可回滚”。
# 只写临时目录，不碰真实安装、不需要管理员权限。
$ErrorActionPreference = 'Stop'
$launcher = Join-Path $PSScriptRoot '..\..\..\target\debug\mesh.exe'
$root = Join-Path $env:TEMP ("mesh-probe-launcher-" + $PID)
$result = Join-Path $env:TEMP ("mesh-probe-launcher-result-" + $PID + '.txt')
$lines = New-Object System.Collections.Generic.List[string]
function New-Bin([string]$dir, [string]$version, [string]$marker) {
    New-Item -ItemType Directory -Force $dir | Out-Null
    ('{"version":"' + $version + '","files":{}}') | Set-Content (Join-Path $dir 'version.json')
    'not a real executable' | Set-Content (Join-Path $dir 'mesh-client.exe')
    'x' | Set-Content (Join-Path $dir $marker)
}
try {
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force $root | Out-Null

    # ---------- A：--sha256 不符 ----------
    $rootA = Join-Path $root 'A'
    New-Item -ItemType Directory -Force $rootA | Out-Null
    $pkgA = Join-Path $root 'user-package-A.tar.zst'
    [System.IO.File]::WriteAllBytes($pkgA, [byte[]](1,2,3,4))
    $outA = & $launcher --file $pkgA --root $rootA --sha256 ('0' * 64) 2>&1
    $codeA = $LASTEXITCODE
    $lines.Add("A: exit=$codeA")
    $lines.Add("A: launcher_output=$($outA -join ' / ')")
    $lines.Add("A: source_package_still_exists=$([bool](Test-Path $pkgA))")

    # ---------- B：上次安装中断（bin.old = 好版本，bin = 坏版本），这次安装又失败 ----------
    $rootB = Join-Path $root 'B'
    New-Item -ItemType Directory -Force $rootB | Out-Null
    New-Bin (Join-Path $rootB 'bin') '2026.11.26-13' 'BROKEN'
    New-Bin (Join-Path $rootB 'bin.old') '2026.11.26-12' 'GOOD'
    $pkgDir = Join-Path $root 'pkg'
    New-Item -ItemType Directory -Force $pkgDir | Out-Null
    '{"version":"2026.11.26-14","files":{"payload.txt":"0000000000000000000000000000000000000000000000000000000000000000"}}' | Set-Content (Join-Path $pkgDir 'version.json')
    'hello' | Set-Content (Join-Path $pkgDir 'payload.txt')
    $pkgB = Join-Path $root 'package-B.tar.zst'
    Push-Location $pkgDir
    try { & tar --zstd -cf $pkgB version.json payload.txt } finally { Pop-Location }
    if (-not (Test-Path $pkgB)) { throw 'tar 没能打出 zstd 包' }
    $outB = & $launcher --file $pkgB --root $rootB --force 2>&1
    $codeB = $LASTEXITCODE
    $lines.Add("B: exit=$codeB")
    $lines.Add("B: launcher_output=$($outB -join ' / ')")
    $lines.Add("B: bin_old_still_exists=$([bool](Test-Path (Join-Path $rootB 'bin.old')))")
    $lines.Add("B: good_marker_survives=$([bool](Test-Path (Join-Path $rootB 'bin\GOOD')))")
    $lines.Add("B: broken_marker_back_in_bin=$([bool](Test-Path (Join-Path $rootB 'bin\BROKEN')))")
    $installed = if (Test-Path (Join-Path $rootB 'bin\version.json')) { (Get-Content (Join-Path $rootB 'bin\version.json') -Raw).Trim() } else { '<none>' }
    $lines.Add("B: bin_version_after=$installed")
    $lines.Add("B: source_package_still_exists=$([bool](Test-Path $pkgB))")
    $lines.Add('B: launcher.log:')
    if (Test-Path (Join-Path $rootB 'log\launcher.log')) { (Get-Content (Join-Path $rootB 'log\launcher.log') -Tail 10) | ForEach-Object { $lines.Add($_) } }
} catch {
    $lines.Add('ERROR: ' + $_.Exception.Message)
} finally {
    $lines | Set-Content $result
    Write-Output "result=$result"
    $lines | ForEach-Object { Write-Output $_ }
    Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
}