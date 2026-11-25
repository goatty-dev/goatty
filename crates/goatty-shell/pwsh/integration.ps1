# goatty shell integration for PowerShell (#278).
# Emits OSC 133 (A, B, C, D;<exit>) prompt marks, OSC 7 working directory URLs,
# checks the reload sentinel (Windows replacement for SIGURG), and binds
# OSC 7337 command-line widgets inside goatty panes.

$global:_goatty_marks = $true
if ((-not $global:_goatty_inside) -and (-not [string]::IsNullOrEmpty($env:VSCODE_SHELL_INTEGRATION))) {
    $global:_goatty_marks = $false
}

function global:_goatty_report_cwd {
    $loc = $executionContext.SessionState.Path.CurrentLocation
    if ($loc.Provider.Name -ne 'FileSystem') { return }
    $p = $loc.ProviderPath.Replace('\', '/')
    if (-not $p.StartsWith('/')) { $p = "/$p" }
    # Percent-encode bytes outside the URL-safe path set.
    $sb = New-Object System.Text.StringBuilder
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($p)
    foreach ($b in $bytes) {
        $ch = [char]$b
        if (($b -ge 0x41 -and $b -le 0x5A) -or ($b -ge 0x61 -and $b -le 0x7A) -or ($b -ge 0x30 -and $b -le 0x39) -or $ch -eq '/' -or $ch -eq '.' -or $ch -eq '_' -or $ch -eq '~' -or $ch -eq '-' -or $ch -eq ':') {
            [void]$sb.Append($ch)
        } else {
            [void]$sb.AppendFormat('%{0:X2}', $b)
        }
    }
    $hostName = $env:COMPUTERNAME
    if ([string]::IsNullOrEmpty($hostName)) { $hostName = [System.Net.Dns]::GetHostName() }
    $esc = [char]27
    [Console]::Write("$esc]7;file://$hostName$($sb.ToString())$esc\")
}

function global:_goatty_check_reload {
    $sentinel = $global:_goatty_reload_sentinel
    if ([string]::IsNullOrEmpty($sentinel) -or -not (Test-Path -LiteralPath $sentinel)) { return }
    try {
        $val = [System.IO.File]::ReadAllText($sentinel).Trim()
        if ($val -ne $global:_goatty_reload_seen) {
            $global:_goatty_reload_seen = $val
            goatty-reload
        }
    } catch {}
}

# Wrap the active prompt (Starship or goatty's fallback) without clobbering it.
# Guard against double-wrapping on `goatty-reload`.
if (($null -eq $global:_goatty_inner_prompt_fn) -or ($function:prompt.ToString() -notlike '*_goatty_prompt_wrapper*')) {
    $global:_goatty_inner_prompt_fn = $function:prompt
}

function global:prompt {
    # marker: _goatty_prompt_wrapper
    $lastOk = $?
    $lastExit = $global:LASTEXITCODE
    $exitCode = if ($lastOk) { 0 } elseif ($null -ne $lastExit -and $lastExit -ne 0) { $lastExit } else { 1 }

    _goatty_check_reload

    $esc = [char]27
    $bel = [char]7
    if ($global:_goatty_running) {
        [Console]::Write("$esc]133;D;$exitCode$bel")
        $global:_goatty_running = $false
    }
    _goatty_report_cwd
    if ($global:_goatty_inside -and $global:_goatty_zle_bound) {
        [Console]::Write("$esc]7337;zle=1$bel")
    }

    # Restore $LASTEXITCODE (and $? via a clean command) before calling the inner prompt (e.g. Starship).
    $global:LASTEXITCODE = $lastExit
    $inner = $global:_goatty_inner_prompt_fn
    $text = if ($inner) {
        if ($lastOk) { $null = $true } else { $global:LASTEXITCODE = $exitCode }
        & $inner
    } else {
        "PS $($executionContext.SessionState.Path.CurrentLocation)> "
    }
    $global:LASTEXITCODE = $lastExit

    if ($global:_goatty_marks) {
        $s = [string]$text
        if (-not $s.Contains("$esc]133;A")) { $s = "$esc]133;A$bel$s" }
        if (-not $s.Contains("$esc]133;B")) { $s = "$s$esc]133;B$bel" }
        return $s
    }
    return $text
}

# Bind Enter to emit OSC 133;C when expansions didn't already bind Enter.
if ($global:_goatty_psreadline) {
    try {
        if (-not $global:_goatty_exp_enter_bound) {
            Set-PSReadLineKeyHandler -Key Enter -ScriptBlock {
                _goatty_accept_line
            } -ErrorAction SilentlyContinue
        }
    } catch {}
}

# --- Command-line actions for goatty's right-click menu (OSC 7337) ----------
$global:_goatty_zle_bound = $false
if ($global:_goatty_inside -and $global:_goatty_psreadline) {
    function global:_goatty_percent_encode([string]$text) {
        $sb = New-Object System.Text.StringBuilder
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($text)
        foreach ($b in $bytes) {
            $ch = [char]$b
            if (($b -ge 0x41 -and $b -le 0x5A) -or ($b -ge 0x61 -and $b -le 0x7A) -or ($b -ge 0x30 -and $b -le 0x39) -or $ch -eq '/' -or $ch -eq '.' -or $ch -eq '_' -or $ch -eq '~' -or $ch -eq ' ' -or $ch -eq '-') {
                [void]$sb.Append($ch)
            } else {
                [void]$sb.AppendFormat('%{0:X2}', $b)
            }
        }
        return $sb.ToString()
    }

    try {
        # 1: report buffer as OSC 7337;line=...
        Set-PSReadLineKeyHandler -Chord 'Escape,[,7,3,3,7,;,1,~' -ScriptBlock {
            $line = $null; $cursor = $null
            [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
            $enc = _goatty_percent_encode ([string]$line)
            $esc = [char]27; $bel = [char]7
            [Console]::Write("$esc]7337;line=$enc$bel")
        } -ErrorAction Stop
        # 2: toggle `sudo ` prefix (Windows 11 sudo / gsudo)
        Set-PSReadLineKeyHandler -Chord 'Escape,[,7,3,3,7,;,2,~' -ScriptBlock {
            $line = $null; $cursor = $null
            [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
            if ([string]::IsNullOrEmpty($line)) {
                [Microsoft.PowerShell.PSConsoleReadLine]::PreviousHistory()
                [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
            }
            if ($line.StartsWith('sudo ')) {
                [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, 5, '')
            } else {
                [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, 0, 'sudo ')
            }
        } -ErrorAction Stop
        # 3: edit command line in $env:VISUAL / $env:EDITOR / notepad
        Set-PSReadLineKeyHandler -Chord 'Escape,[,7,3,3,7,;,3,~' -ScriptBlock {
            $line = $null; $cursor = $null
            [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
            $editor = if (-not [string]::IsNullOrEmpty($env:VISUAL)) { $env:VISUAL } elseif (-not [string]::IsNullOrEmpty($env:EDITOR)) { $env:EDITOR } else { 'notepad.exe' }
            $tmp = [System.IO.Path]::Combine([System.IO.Path]::GetTempPath(), "goatty-edit-$PID.ps1")
            $utf8 = New-Object System.Text.UTF8Encoding($false)
            [System.IO.File]::WriteAllText($tmp, [string]$line, $utf8)
            $esc = [char]27; $bel = [char]7
            [Console]::Write("$esc]7337;zle=0$bel")
            try {
                & $editor $tmp
                if (Test-Path -LiteralPath $tmp) {
                    $edited = [System.IO.File]::ReadAllText($tmp, $utf8).TrimEnd("`r", "`n")
                    [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $line.Length, $edited)
                }
            } finally {
                Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
                [Console]::Write("$esc]7337;zle=1$bel")
            }
        } -ErrorAction Stop
        # 4: clear command line
        Set-PSReadLineKeyHandler -Chord 'Escape,[,7,3,3,7,;,4,~' -ScriptBlock {
            [Microsoft.PowerShell.PSConsoleReadLine]::RevertLine()
        } -ErrorAction Stop
        $global:_goatty_zle_bound = $true
    } catch {
        $global:_goatty_zle_bound = $false
    }
}
