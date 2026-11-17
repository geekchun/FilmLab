# FilmLab Windows 安装包构建：cargo build -> NSIS 打包
# 用法：powershell -ExecutionPolicy Bypass -File packaging\windows\build-installer.ps1 [-SkipBuild]
# 前置：NSIS 3（winget install NSIS.NSIS）。产物 dist\FilmLab-<版本>-setup.exe。
param([switch]$SkipBuild)

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$root = Split-Path -Parent (Split-Path -Parent $here)

# 版本取根 Cargo.toml 的 [workspace.package] version，避免 exe 与安装包两处维护
$cargo = Get-Content (Join-Path $root 'Cargo.toml') -Raw
if ($cargo -notmatch '(?ms)^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"') {
    throw '无法从 Cargo.toml 的 [workspace.package] 读取 version'
}
$version = $Matches[1]

$exe  = Join-Path $root 'target\release\filmlab.exe'
$icon = Join-Path $root 'crates\filmlab\assets\icon.ico'
$nsi  = Join-Path $here 'FilmLab.nsi'
$dist = Join-Path $root 'dist'
$out  = Join-Path $dist "FilmLab-$version-setup.exe"

if (-not $SkipBuild) {
    Write-Host '==> cargo build --release -p filmlab'
    # 静态 CRT：未装 VC++ 运行库的机器也能直接启动，否则 exe 依赖 VCRUNTIME140*.dll
    $env:RUSTFLAGS = '-C target-feature=+crt-static'
    Push-Location $root
    cargo build --release -p filmlab
    Pop-Location
}

if (-not (Test-Path $exe))  { throw "找不到 $exe" }
if (-not (Test-Path $icon)) { throw "找不到 $icon" }

# makensis：先查 PATH，再看常见安装位置
$mk = (Get-Command makensis -ErrorAction SilentlyContinue).Source
if (-not $mk) {
    $mk = @(
        "$env:ProgramFiles\NSIS\makensis.exe",
        "${env:ProgramFiles(x86)}\NSIS\makensis.exe",
        "$env:LOCALAPPDATA\Programs\NSIS\makensis.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if (-not $mk) { throw '未找到 makensis，请先安装 NSIS 3：winget install NSIS.NSIS' }

New-Item -ItemType Directory -Force -Path $dist | Out-Null
Remove-Item $out -Force -ErrorAction SilentlyContinue

Write-Host "==> makensis (FilmLab $version)"
& $mk '-INPUTCHARSET' 'UTF8' "-DAPP_VERSION=$version" "-DAPP_VERSION4=$version.0" `
      "-DSOURCE_EXE=`"$exe`"" "-DAPP_ICON=`"$icon`"" "-DOUTFILE=`"$out`"" $nsi
if ($LASTEXITCODE -ne 0) { throw "makensis 失败 (exit $LASTEXITCODE)" }

Write-Host ("`n完成: {0} ({1:N2} MB)" -f $out, ((Get-Item $out).Length / 1MB))
