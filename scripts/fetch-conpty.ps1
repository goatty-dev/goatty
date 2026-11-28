# Fetch Microsoft's ConPTY (the Microsoft.Windows.Console.ConPTY NuGet
# package, MIT, from microsoft/terminal) into OutDir, the folder that holds
# (or will hold) goatty.exe and goattyd.exe. goatty-term loads conpty.dll from
# its executable's folder when it's there (crates/goatty-term/src/pty/conpty.rs);
# without it, goatty uses the older ConPTY built into Windows, which strips
# image protocols and breaks TUI screen restore (#356).
#
# OutDir gets:
#   conpty.dll              for the build's architecture
#   x64\OpenConsole.exe     conpty.dll starts OpenConsole.exe from the folder
#   arm64\OpenConsole.exe   named after the *machine's* architecture, and
#                           silently falls back to the in-box conhost.exe if
#                           it's missing; an x64 goatty also runs on ARM64
#   LICENSE-ConPTY.txt      Microsoft's MIT notice (packaging/windows)
#
# The package is pinned by version and SHA-256, and on Windows every binary
# must carry a valid Authenticode signature from Microsoft Corporation. The
# binaries are copied as they are: never re-sign them.
#
#   pwsh scripts/fetch-conpty.ps1 -OutDir target\dist      # release.yml
#   pwsh scripts/fetch-conpty.ps1 -OutDir target\debug     # a dev build
#
# To update: pick a version from
# https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/index.json
# (1.25.260930003 or later: older builds have TUI screen-restore regressions),
# download its .nupkg and put its sha256sum below.
param(
    [Parameter(Mandatory)] [string]$OutDir,
    # x86_64 or aarch64; defaults to this machine's.
    [string]$Arch = ""
)
$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$ProgressPreference = "SilentlyContinue"

$version = "1.25.261109001"
$sha256 = "3d77f535df848e44225c2a8912ac844fecb23f3057732b639f3034b587496f09"

if (-not $Arch) {
    $Arch = switch ($env:PROCESSOR_ARCHITECTURE) {
        "AMD64" { "x86_64" }
        "ARM64" { "aarch64" }
        default { throw "unsupported architecture: $env:PROCESSOR_ARCHITECTURE (pass -Arch)" }
    }
}
$dll, $hosts = switch ($Arch) {
    "x86_64" { "win-x64", @("x64", "arm64") }
    "aarch64" { "win-arm64", @("arm64") }
    default { throw "unsupported -Arch $Arch (x86_64 or aarch64)" }
}

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$work = Join-Path ([IO.Path]::GetTempPath()) "goatty-conpty-$PID"
New-Item -ItemType Directory -Force $work | Out-Null
try {
    $id = "microsoft.windows.console.conpty"
    $nupkg = Join-Path $work "$id.$version.nupkg"
    $url = "https://api.nuget.org/v3-flatcontainer/$id/$version/$id.$version.nupkg"
    Write-Output "fetching $url"
    Invoke-WebRequest -Uri $url -OutFile $nupkg -UseBasicParsing
    $got = (Get-FileHash -Algorithm SHA256 $nupkg).Hash.ToLowerInvariant()
    if ($got -ne $sha256) { throw "$id $version has SHA-256 $got, expected $sha256" }

    $pkg = Join-Path $work "pkg"
    # A .nupkg is a zip; Expand-Archive only takes the .zip extension.
    $zip = [IO.Path]::ChangeExtension($nupkg, ".zip")
    Move-Item $nupkg $zip
    Expand-Archive $zip $pkg

    # Published path => path in the package.
    $files = [ordered]@{ "conpty.dll" = "runtimes\$dll\native\conpty.dll" }
    foreach ($h in $hosts) { $files["$h\OpenConsole.exe"] = "build\native\runtimes\$h\OpenConsole.exe" }

    if ($env:OS -eq "Windows_NT" -or $IsWindows) {
        foreach ($src in $files.Values) {
            $path = Join-Path $pkg $src
            $sig = Get-AuthenticodeSignature $path
            if ($sig.Status -ne "Valid") { throw "$src isn't validly signed: $($sig.Status) $($sig.StatusMessage)" }
            $subject = $sig.SignerCertificate.Subject
            if ($subject -notmatch '(^|,\s*)CN=Microsoft Corporation(,|$)') { throw "$src is signed by '$subject', not Microsoft Corporation" }
            Write-Output "${src}: signed by $subject"
        }
    }
    else {
        Write-Warning "not on Windows: Authenticode signatures not checked (the package's SHA-256 was)"
    }

    New-Item -ItemType Directory -Force $OutDir | Out-Null
    foreach ($dest in $files.Keys) {
        $to = Join-Path $OutDir $dest
        New-Item -ItemType Directory -Force (Split-Path $to) | Out-Null
        Copy-Item (Join-Path $pkg $files[$dest]) $to -Force
        Write-Output $to
    }
    Copy-Item (Join-Path $root "packaging\windows\LICENSE-ConPTY.txt") $OutDir -Force
    Write-Output (Join-Path $OutDir "LICENSE-ConPTY.txt")
}
finally {
    Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}
