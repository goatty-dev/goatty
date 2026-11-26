# Download and install goatty on Windows from a GitHub release, without
# administrator rights:
#
#   irm https://<website>/install.ps1 | iex
#
# or a given release, without the prompt for running sessions:
#
#   & ([scriptblock]::Create((irm https://<website>/install.ps1))) -Version 0.4.0 -Force
#
# (or straight from GitHub: https://raw.githubusercontent.com/goatty-dev/goatty/main/scripts/get-goatty.ps1)
#
# It downloads the release's per-user MSI, checks it against the release's
# SHA256SUMS and installs it with a progress window and no other prompts: into
# %LOCALAPPDATA%\Programs\goatty, on PATH and in the Start Menu. Running it
# again upgrades. A file downloaded here has no mark of the web, so SmartScreen
# doesn't stop the unsigned installer. The website's /install.ps1 redirects to
# this file on main (goatty-website public/_redirects), so keep it working for
# every published release, in Windows PowerShell 5.1 too. scripts/get-goatty.sh
# is the Linux and macOS counterpart.
#
# Everything runs from Install-Goatty at the end, so a download cut short runs
# nothing.
param(
    # Install this release (e.g. 0.4.0) instead of the latest.
    [string]$Version = '',
    # Don't ask before an upgrade ends the running goattyd's sessions.
    [switch]$Force
)

function Install-Goatty {
    param([string]$Version, [bool]$Force)
    $ErrorActionPreference = 'Stop'
    # Invoke-WebRequest's progress bar makes downloads many times slower in 5.1.
    $ProgressPreference = 'SilentlyContinue'
    # Windows PowerShell 5.1 may not offer TLS 1.2 by default; GitHub needs it.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $repo = 'goatty-dev/goatty'
    $Version = $Version.TrimStart('v')

    if ([Environment]::OSVersion.Platform -ne 'Win32NT') {
        throw "This installer is for Windows. On Linux and macOS run: curl -fsSL https://raw.githubusercontent.com/$repo/main/scripts/get-goatty.sh | sh"
    }
    # The architecture of the OS, not of this (possibly 32-bit) PowerShell.
    $arch = $env:PROCESSOR_ARCHITEW6432
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    switch ($arch) {
        'AMD64' { }
        'ARM64' { Write-Host 'goatty: no ARM64 build yet; installing the x86_64 build, which Windows 11 runs emulated.' }
        default { throw "goatty has no build for $arch Windows." }
    }

    if ($Version) {
        $url = "https://github.com/$repo/releases/download/v$Version"
        Write-Host "goatty: looking up goatty $Version"
    } else {
        $url = "https://github.com/$repo/releases/latest/download"
        Write-Host 'goatty: looking up the latest goatty release'
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ('goatty-install-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $sums = Join-Path $tmp 'SHA256SUMS'
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$url/SHA256SUMS" -OutFile $sums
        } catch {
            $hint = if ($Version) { " (is $Version a goatty release?)" } else { '' }
            throw "Couldn't download $url/SHA256SUMS$hint`: $($_.Exception.Message)"
        }
        # Lines are "<sha256>  <file>"; the MSI is goatty-<version>-x86_64-windows.msi.
        $line = Get-Content $sums | Where-Object { $_ -match '^([0-9a-f]{64})  (goatty-\S+-x86_64-windows\.msi)$' } | Select-Object -First 1
        if (-not $line) { throw "This release has no Windows installer. See https://github.com/$repo/releases" }
        $null = $line -match '^([0-9a-f]{64})  (\S+)$'
        $want = $Matches[1]
        $msiName = $Matches[2]
        $msi = Join-Path $tmp $msiName

        # The MSI stops the installed goattyd before it replaces it, and on
        # Windows its sessions can't outlive it (docs/releasing.md).
        $running = @(Get-Process -Name goattyd -ErrorAction SilentlyContinue)
        if ($running.Count -gt 0 -and -not $Force) {
            Write-Host 'goatty: goattyd is running. Upgrading stops it, which ends the shells in your goatty sessions.'
            $answer = Read-Host 'Continue? [y/N]'
            if ($answer -notmatch '^(y|yes)$') { Write-Host 'goatty: nothing installed.'; return }
        }

        Write-Host "goatty: downloading $msiName"
        Invoke-WebRequest -UseBasicParsing -Uri "$url/$msiName" -OutFile $msi
        $got = (Get-FileHash -Algorithm SHA256 -Path $msi).Hash.ToLowerInvariant()
        if ($got -ne $want) { throw "$msiName doesn't match SHA256SUMS (got $got, expected $want); try again." }

        Write-Host 'goatty: installing'
        $log = Join-Path $tmp 'install.log'
        $p = Start-Process -FilePath msiexec.exe -Wait -PassThru `
            -ArgumentList @('/i', "`"$msi`"", '/passive', '/norestart', '/l*v', "`"$log`"")
        switch ($p.ExitCode) {
            0 { }
            3010 { Write-Host 'goatty: installed; Windows asks for a restart to finish.' }
            1602 { Write-Host 'goatty: the installation was cancelled.'; return }
            default {
                $keep = Join-Path ([IO.Path]::GetTempPath()) 'goatty-install.log'
                Copy-Item $log $keep -Force -ErrorAction SilentlyContinue
                throw "The installer failed (msiexec exit code $($p.ExitCode)). Its log is $keep"
            }
        }
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }

    # The MSI put goatty on the user's PATH; make this session see it too.
    $dir = Join-Path $env:LOCALAPPDATA 'Programs\goatty'
    if (($env:Path -split ';') -notcontains $dir) { $env:Path = "$env:Path;$dir" }
    # goatty.exe is a GUI program: it writes only to a pipe, not to the console.
    $installed = (& (Join-Path $dir 'goatty.exe') --version 2>$null | Out-String).Trim()
    if (-not $installed) { $installed = $msiName }
    Write-Host "goatty: installed $installed. Start it from the Start Menu or run: goatty"
}

Install-Goatty -Version $Version -Force $Force.IsPresent
