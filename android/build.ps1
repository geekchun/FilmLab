# FilmLab 安卓构建脚本：Rust cdylib → dex → 资源 → APK → 签名。
#
# 不依赖 Gradle：aapt2 / d8 / zipalign / apksigner 手工串起来，产物在 android/dist。
# 前置：JDK 17、Android SDK(platform-34 / build-tools 34.0.0 / NDK 26.1)、
#       rustup target add aarch64-linux-android。
#
# 用法：powershell -ExecutionPolicy Bypass -File android\build.ps1 [-SkipRust]

param([switch]$SkipRust)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$dev  = "$env:USERPROFILE\dev"
$jdk  = "$dev\jdk17"
$sdk  = "$dev\sdk"
$ndk  = "$env:USERPROFILE\dev\sdk\ndk\26.1.10909125\toolchains\llvm\prebuilt\windows-x86_64"

if (-not (Test-Path $jdk)) { throw "找不到 JDK: $jdk" }
if (-not (Test-Path $sdk)) { throw "找不到 Android SDK: $sdk" }
if (-not (Test-Path "$ndk\bin\clang.exe")) { throw "找不到 NDK clang: $ndk" }

$env:JAVA_HOME = $jdk
$env:Path = "$jdk\bin;" + $env:Path
$env:CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER = "$ndk\bin\clang.exe"
$env:RUSTFLAGS = "-C link-arg=--target=aarch64-linux-android26 -C link-arg=--sysroot=$ndk\sysroot"

$bt  = Join-Path $sdk 'build-tools\34.0.0'
$jar = Join-Path $sdk 'platforms\android-34\android.jar'
$app = Join-Path $PSScriptRoot 'app'
$out = Join-Path $PSScriptRoot 'build'
$dist = Join-Path $PSScriptRoot 'dist'
$assets = Join-Path $app 'src\main\assets'

function Step($name, $block) {
    Write-Host "==> $name"
    & $block
    if ($LASTEXITCODE -ne 0 -and $null -ne $LASTEXITCODE) {
        throw "$name 失败 (exit $LASTEXITCODE)"
    }
}

New-Item -ItemType Directory -Force -Path $out, $dist, $assets | Out-Null
Remove-Item "$out\classes", "$out\dexout", "$out\base.apk", "$out\unsigned.apk", "$out\aligned.apk" -Recurse -Force -ErrorAction SilentlyContinue

if (-not $SkipRust) {
    Step 'Rust (arm64-v8a)' {
        Push-Location $root
        cargo build -p film-android --release --target aarch64-linux-android
        Pop-Location
    }
}

Step '复制前端资源' {
    Copy-Item (Join-Path $root 'crates\filmlab\assets\ui.html') (Join-Path $assets 'ui.html') -Force
}

Step 'javac' {
    New-Item -ItemType Directory -Force -Path "$out\classes" | Out-Null
    $src = Get-ChildItem "$app\src\main\java" -Recurse -Filter *.java | ForEach-Object { $_.FullName }
    & "$jdk\bin\javac.exe" -source 8 -target 8 -classpath $jar -encoding UTF-8 -d "$out\classes" $src
}

Step 'd8' {
    New-Item -ItemType Directory -Force -Path "$out\dexout" | Out-Null
    $cls = Get-ChildItem "$out\classes" -Recurse -Filter *.class | ForEach-Object { $_.FullName }
    & "$bt\d8.bat" --release --min-api 26 --lib $jar --output "$out\dexout" $cls
}

Step 'aapt2 compile' {
    & "$bt\aapt2.exe" compile --dir (Join-Path $app 'src\main\res') -o "$out\res.zip"
}

Step 'aapt2 link' {
    & "$bt\aapt2.exe" link -o "$out\base.apk" -I $jar `
        --manifest "$app\src\main\AndroidManifest.xml" -A $assets --auto-add-overlay `
        "$out\res.zip"
}

Step '装入 dex 与 so' {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    Copy-Item "$out\base.apk" "$out\unsigned.apk" -Force
    $zip = [System.IO.Compression.ZipFile]::Open("$out\unsigned.apk", 'Update')
    $files = @(
        @{ n = 'classes.dex'; p = "$out\dexout\classes.dex"; l = [System.IO.Compression.CompressionLevel]::Optimal },
        @{ n = 'lib/arm64-v8a/libfilm_android.so'; p = "$root\target\aarch64-linux-android\release\libfilm_android.so"; l = [System.IO.Compression.CompressionLevel]::NoCompression }
    )
    foreach ($f in $files) {
        $e = $zip.CreateEntry($f.n, $f.l)
        $s = $e.Open()
        $bytes = [System.IO.File]::ReadAllBytes($f.p)
        $s.Write($bytes, 0, $bytes.Length)
        $s.Dispose()
    }
    $zip.Dispose()
}

Step 'zipalign' {
    & "$bt\zipalign.exe" -f -p 4 "$out\unsigned.apk" "$out\aligned.apk"
}

$ks = Join-Path $PSScriptRoot 'debug.keystore'
if (-not (Test-Path $ks)) {
    Step '生成调试签名' {
        & "$jdk\bin\keytool.exe" -genkeypair -keystore $ks -alias androiddebugkey `
            -storepass android -keypass android -keyalg RSA -keysize 2048 -validity 10000 `
            -dname "CN=FilmLab,O=FilmLab,C=CN"
    }
}

$apk = Join-Path $dist 'FilmLab-0.1.0.apk'
Remove-Item $apk, "$apk.idsig" -Force -ErrorAction SilentlyContinue
Step '签名' {
    & "$bt\apksigner.bat" sign --ks $ks --ks-pass pass:android --key-pass pass:android `
        --out $apk "$out\aligned.apk"
}

Write-Host ("`n完成: {0} ({1:N2} MB)" -f $apk, ((Get-Item $apk).Length / 1MB))
