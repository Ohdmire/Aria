<#
  构建 Aria 的 Windows 安装包(NSIS,固实 LZMA 压缩)。

  用法:
    powershell -ExecutionPolicy Bypass -File packaging\windows\build-installer.ps1
    powershell ... -SkipBuild          只打包,不重新编译(exe 需已存在)
    powershell ... -Nsis "D:\nsis\makensis.exe"   指定 makensis 路径

  产物:dist\aria-<版本>-win64-setup.exe
#>
param(
    [switch]$SkipBuild,
    [string]$Nsis = "C:\Program Files (x86)\NSIS\makensis.exe"
)

$ErrorActionPreference = "Stop"

# PowerShell 5.1 在 Stop 下会把 native 程序写到 stderr 的内容(哪怕只是 warning)
# 当成终止错误;跑外部命令时临时放宽,只认退出码。
function Invoke-External {
    param([scriptblock]$Command)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & $Command 2>&1 | ForEach-Object { Write-Host $_ }
        return $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $prev
    }
}

if (-not (Test-Path $Nsis)) {
    throw "找不到 makensis: $Nsis(先装 NSIS:winget install NSIS.NSIS)"
}

$root = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path

# 版本号只从 Cargo.toml 读,和程序内「关于」显示的保持一致
$m = Select-String -Path (Join-Path $root "Cargo.toml") -Pattern '^\s*version\s*=\s*"([^"]+)"' -Encoding utf8 | Select-Object -First 1
if (-not $m) { throw "Cargo.toml 里读不到 version" }
$ver = $m.Matches[0].Groups[1].Value

if (-not $SkipBuild) {
    Write-Host "==> cargo build --release" -ForegroundColor Cyan
    $code = Invoke-External { cargo build --release --manifest-path (Join-Path $root "Cargo.toml") }
    if ($code -ne 0) { throw "cargo build --release 失败(退出码 $code)" }
}

$exe = Join-Path $root "target\release\aria.exe"
if (-not (Test-Path $exe)) { throw "找不到 $exe(先 cargo build --release)" }

$dist = Join-Path $root "dist"
New-Item -ItemType Directory -Force -Path $dist | Out-Null

$out = Join-Path $dist "aria-$ver-win64-setup.exe"
if (Test-Path $out) { Remove-Item $out -Force }

Write-Host "==> makensis (v$ver)" -ForegroundColor Cyan
$code = Invoke-External {
    & $Nsis "/DAPPVERSION=$ver" "/DROOT=$root" "/DOUTDIR=$dist" "/INPUTCHARSET" "UTF8" (Join-Path $PSScriptRoot "aria.nsi")
}
if ($code -ne 0) { throw "makensis 失败(退出码 $code)" }

$size = (Get-Item $out).Length
Write-Host ""
Write-Host ("==> 完成: {0}" -f $out) -ForegroundColor Green
Write-Host ("    大小: {0:N1} MB" -f ($size / 1MB))
