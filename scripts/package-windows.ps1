# Package a Windows dist build into OutDir (default dist\): the portable
# archive goatty-<version>-x86_64-windows.zip, the per-user installer
# goatty-<version>-x86_64-windows.msi (packaging/windows/goatty.wxs) and
# goattyd-x86_64-windows.exe for remote Windows hosts. release.yml runs this;
# locally (PowerShell 7, WiX 5 with the Util and UI extensions, see release.yml):
#   cargo build --profile dist --locked -p goatty-app -p goatty-daemon
#   # static goattyds for remote hosts in remote\goattyd-<arch>-linux (see release.yml)
#   pwsh scripts/package-windows.ps1
# -MsiVersion builds only the MSI, with another version (release.yml's
# upgrade test).
param(
    [string]$OutDir = "dist",
    [string]$MsiVersion = ""
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$PSNativeCommandUseErrorActionPreference = $true

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $root
$bin = "target\dist"

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$arch, $wixArch = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64", "x64" }
    "ARM64" { "aarch64", "arm64" }
    default { throw "unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}

foreach ($b in "goatty", "goattyd") {
    $exe = Join-Path $bin "$b.exe"
    if (-not (Test-Path $exe)) {
        throw "missing $exe; build with: cargo build --profile dist --locked -p goatty-app -p goatty-daemon"
    }
    # Through a pipe: goatty.exe is a GUI-subsystem binary and only writes to
    # inherited handles.
    $got = (& $exe --version | Out-String).Trim()
    if ($got -ne "$b $version") { throw "$exe --version says '$got', expected '$b $version'" }
}
$remotes = @(Get-ChildItem remote -Filter "goattyd-*-linux" -ErrorAction SilentlyContinue)
foreach ($r in "goattyd-x86_64-linux", "goattyd-aarch64-linux") {
    if (-not (Test-Path (Join-Path remote $r))) { throw "missing remote\$r (static goattyds for remote hosts)" }
}

New-Item -ItemType Directory -Force $OutDir | Out-Null
$OutDir = Resolve-Path $OutDir
$name = "goatty-$version-$arch-windows"
$stage = Join-Path ([IO.Path]::GetTempPath()) "goatty-package-$PID"
$dir = Join-Path $stage $name
New-Item -ItemType Directory -Force (Join-Path $dir "remote") | Out-Null
try {
    Copy-Item "$bin\goatty.exe", "$bin\goattyd.exe", README.md, LICENSE-MIT, LICENSE-APACHE $dir
    Copy-Item $remotes.FullName (Join-Path $dir "remote")

    # MSI versions are numeric: a pre-release (0.3.0-rc.1) installs as 0.3.0.
    $numeric = if ($MsiVersion) { $MsiVersion } else { $version -replace '-.*$', '' }
    $msiName = if ($MsiVersion) { "goatty-$MsiVersion-$arch-windows.msi" } else { "$name.msi" }
    wix build packaging\windows\goatty.wxs -arch $wixArch `
        -ext WixToolset.Util.wixext -ext WixToolset.UI.wixext `
        -d "Version=$numeric" -d "Source=$dir" -d "Packaging=$root\packaging\windows" `
        -o (Join-Path $OutDir $msiName)
    Write-Output (Join-Path $OutDir $msiName)
    if ($MsiVersion) { return }

    $zip = Join-Path $OutDir "$name.zip"
    Remove-Item $zip -ErrorAction SilentlyContinue
    Compress-Archive -Path $dir -DestinationPath $zip
    Write-Output $zip

    $standalone = Join-Path $OutDir "goattyd-$arch-windows.exe"
    Copy-Item "$bin\goattyd.exe" $standalone
    Write-Output $standalone
    # The .wixpdb next to the MSI is debugging data, not a release asset.
    Remove-Item (Join-Path $OutDir "*.wixpdb") -ErrorAction SilentlyContinue
}
finally {
    Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
}
