# Test the Windows release packages on a clean machine (release.yml's
# test-windows job; it changes the user's PATH, Start Menu and goatty install,
# so don't run it on your own computer):
#   1. the portable .zip runs from wherever it's extracted;
#   2. the MSI upgrades an older install while that install's goattyd is
#      running (it must stop it, without asking for a reboot) and replaces it
#      instead of installing a second copy;
#   3. the installed goatty works, is on PATH and has its Start Menu shortcut;
#   4. uninstalling removes all of it.
param(
    [Parameter(Mandatory)] [string]$Packages,
    [Parameter(Mandatory)] [string]$UpgradeTest
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$version = (Select-String -Path (Join-Path $root Cargo.toml) -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
$upgradeCode = "{DCFDA8F6-615A-4ABF-9125-1EE2ABE2A257}" # packaging/windows/goatty.wxs
$dir = Join-Path $env:LOCALAPPDATA "Programs\goatty"
$shortcut = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\goatty.lnk"

function Step($text) { Write-Output "`n=== $text" }

# Runs a binary and returns its output; goatty.exe (GUI subsystem) only
# writes to inherited handles, so always through a pipe.
function Run($exe) {
    $out = & $exe @args 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) { throw "$exe $args exited with $LASTEXITCODE`n$out" }
    $out.Trim()
}

function Check-Version($exe, $name) {
    $got = Run $exe --version
    if ($got -ne "$name $version") { throw "$exe --version says '$got', expected '$name $version'" }
    Write-Output "${exe}: $got"
}

function Msiexec($verb, $msi) {
    $log = "$msi.$verb.log"
    $p = Start-Process msiexec.exe -ArgumentList "/$verb", "`"$msi`"", "/qn", "/norestart", "/l*v", "`"$log`"" -Wait -PassThru
    # 3010: succeeded, but a file was in use and needs a reboot to be replaced.
    if ($p.ExitCode -ne 0) {
        Get-Content $log -Tail 120
        throw "msiexec /$verb $msi exited with $($p.ExitCode)"
    }
    Write-Output "msiexec /$verb ${msi}: ok"
}

# Product codes of the installed MSIs with goatty's UpgradeCode. PowerShell
# unrolls what a function returns, so callers wrap it in @().
function Installed-Products {
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $installer.RelatedProducts($upgradeCode)
}

function Start-Goattyd {
    $p = Start-Process (Join-Path $dir "goattyd.exe") -WindowStyle Hidden -PassThru
    $deadline = (Get-Date).AddSeconds(20)
    while ($true) {
        & (Join-Path $dir "goattyd.exe") list *> $null
        if ($LASTEXITCODE -eq 0) { break }
        if ($p.HasExited) { throw "goattyd exited with $($p.ExitCode)" }
        if ((Get-Date) -gt $deadline) { throw "goattyd didn't start listening" }
        Start-Sleep -Milliseconds 200
    }
    Write-Host "goattyd running (pid $($p.Id))"
    $p
}

Step "portable archive"
$zip = Get-ChildItem $Packages -Filter "goatty-$version-x86_64-windows.zip" | Select-Object -First 1
if (-not $zip) { throw "no goatty-$version-x86_64-windows.zip in $Packages" }
$portable = Join-Path ([IO.Path]::GetTempPath()) "goatty-portable"
Expand-Archive $zip.FullName $portable -Force
$pdir = Join-Path $portable "goatty-$version-x86_64-windows"
Check-Version (Join-Path $pdir "goatty.exe") goatty
Check-Version (Join-Path $pdir "goattyd.exe") goattyd
foreach ($r in "goattyd-x86_64-linux", "goattyd-aarch64-linux") {
    if (-not (Test-Path (Join-Path $pdir "remote\$r"))) { throw "the archive has no remote\$r" }
}
Check-Version (Get-ChildItem $Packages -Filter "goattyd-x86_64-windows.exe").FullName goattyd

Step "install an older version and start its goattyd"
$old = (Get-ChildItem $UpgradeTest -Filter "*.msi" | Select-Object -First 1).FullName
$new = (Get-ChildItem $Packages -Filter "goatty-$version-x86_64-windows.msi" | Select-Object -First 1).FullName
Msiexec i $old
$daemon = Start-Goattyd

Step "upgrade"
Msiexec i $new
if (-not $daemon.WaitForExit(10000)) { throw "the upgrade didn't stop the running goattyd" }
$products = @(Installed-Products)
if ($products.Count -ne 1) { throw "expected one installed goatty, found $($products.Count): $products" }
Write-Output "installed: $products"

Step "installed files"
Check-Version (Join-Path $dir "goatty.exe") goatty
Check-Version (Join-Path $dir "goattyd.exe") goattyd
foreach ($r in "goattyd-x86_64-linux", "goattyd-aarch64-linux") {
    if (-not (Test-Path (Join-Path $dir "remote\$r"))) { throw "no remote\$r installed" }
}
$completions = Run (Join-Path $dir "goatty.exe") completions powershell
if ($completions -notmatch "Register-ArgumentCompleter") { throw "goatty completions powershell printed no completer" }

Step "PATH and Start Menu"
$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
Write-Output "user PATH: $userPath"
if (-not ($userPath -split ";" | Where-Object { $_.TrimEnd("\") -eq $dir })) { throw "$dir isn't on the user's PATH" }
if (-not (Test-Path $shortcut)) { throw "no Start Menu shortcut at $shortcut" }
$shell = New-Object -ComObject Shell.Application
$lnk = $shell.Namespace((Split-Path $shortcut)).ParseName((Split-Path $shortcut -Leaf))
$aumid = $lnk.ExtendedProperty("System.AppUserModel.ID")
if ($aumid -ne "dev.goatty.goatty") { throw "the shortcut's AppUserModel.ID is '$aumid', expected dev.goatty.goatty" }
Write-Output "shortcut: $shortcut ($aumid)"

Step "uninstall"
$daemon = Start-Goattyd
Msiexec x $new
if (-not $daemon.WaitForExit(10000)) { throw "uninstalling didn't stop the running goattyd" }
if (Test-Path $dir) { throw "$dir is still there: $(Get-ChildItem -Recurse $dir)" }
if (Test-Path $shortcut) { throw "the Start Menu shortcut is still there" }
$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
if ($userPath -and ($userPath -split ";" | Where-Object { $_.TrimEnd("\") -eq $dir })) { throw "$dir is still on the user's PATH" }
if (@(Installed-Products).Count -ne 0) { throw "goatty is still registered as installed" }
Write-Output "`nall good"
