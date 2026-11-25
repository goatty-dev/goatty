# goatty's built-in fzf plugin for PowerShell (PS 7 and Windows PowerShell 5.1).
# Sets FZF_DEFAULT_OPTS_FILE to goatty's themed fzfrc, probes optional helpers
# (fd, bat, eza), and binds Ctrl+R (history), Ctrl+T (files), and Alt+C (dirs)
# via PSReadLine without requiring any third-party PowerShell modules.

function script:_goatty_fzf_export([string]$Name, [string]$Value) {
    $mineKey = "_goatty_fzf_$Name"
    $cur = [System.Environment]::GetEnvironmentVariable($Name)
    $mine = [System.Environment]::GetEnvironmentVariable($mineKey)
    if ([string]::IsNullOrEmpty($cur) -or ((-not [string]::IsNullOrEmpty($mine)) -and ($cur -eq $mine))) {
        Set-Item -Path "Env:$Name" -Value $Value
        Set-Item -Path "Env:$mineKey" -Value $Value
    }
}

function script:_goatty_fzf_helper([string]$Name, [string[]]$Candidates) {
    foreach ($c in $Candidates) {
        $cmd = Get-Command -Name $c -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($cmd -and $cmd.Source) {
            $global:_goatty_state["helper:$Name"] = "ok`t$($cmd.Source)"
            return $cmd.Source
        }
    }
    $global:_goatty_state["helper:$Name"] = 'missing'
    return $null
}

_goatty_fzf_export 'FZF_DEFAULT_OPTS_FILE' (Join-Path $goatty_plugin_dir 'fzfrc')

$_goatty_fzf_hidden = ($goatty_settings['hidden_files'] -eq 'true')
$_goatty_fzf_previews = ($goatty_settings['previews'] -eq 'true')
$_goatty_fzf_fd = _goatty_fzf_helper 'fd' @('fd', 'fdfind')
$_goatty_fzf_bat = $null
$_goatty_fzf_eza = $null
if ($_goatty_fzf_previews) {
    $_goatty_fzf_bat = _goatty_fzf_helper 'bat' @('bat', 'batcat')
    $_goatty_fzf_eza = _goatty_fzf_helper 'eza' @('eza')
}

$global:_goatty_fzf_cfg = @{
    hidden   = $_goatty_fzf_hidden
    previews = $_goatty_fzf_previews
    fd       = $_goatty_fzf_fd
    bat      = $_goatty_fzf_bat
    eza      = $_goatty_fzf_eza
}

function global:_goatty_ps_quote_arg([string]$s) {
    if ([string]::IsNullOrEmpty($s)) { return "''" }
    if ($s -match '^[A-Za-z0-9_./=+\\:-]+$' -and (-not $s.StartsWith('.')) -and (-not $s.StartsWith('-'))) {
        return $s
    }
    return "'" + $s.Replace("'", "''") + "'"
}

if ($global:_goatty_psreadline) {
    # Ctrl+R: fuzzy search PSReadLine history (multiline-aware, newest first, deduplicated).
    Set-PSReadLineKeyHandler -Chord 'Ctrl+r' -BriefDescription 'goatty fzf history search' -ScriptBlock {
        param($key, $arg)
        $line = $null
        $cursor = $null
        [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
        $histPath = (Get-PSReadLineOption).HistorySavePath
        if ([string]::IsNullOrEmpty($histPath) -or (-not (Test-Path -LiteralPath $histPath))) {
            return
        }
        $raw = [System.IO.File]::ReadAllLines($histPath)
        # Reconstruct PSReadLine multiline records (physical lines ending with `` ` `` continue onto the next line).
        $cmds = New-Object System.Collections.Generic.List[string]
        $buf = New-Object System.Text.StringBuilder
        foreach ($phys in $raw) {
            if ($phys.EndsWith('`')) {
                [void]$buf.Append($phys.Substring(0, $phys.Length - 1))
                [void]$buf.Append("`n")
            } else {
                [void]$buf.Append($phys)
                $cmds.Add($buf.ToString())
                [void]$buf.Clear()
            }
        }
        if ($buf.Length -gt 0) {
            $cmds.Add($buf.ToString().TrimEnd("`n"))
        }

        $seen = New-Object 'System.Collections.Generic.HashSet[string]' ([System.StringComparer]::Ordinal)
        $unique = New-Object System.Collections.Generic.List[string]
        $items = New-Object System.Collections.Generic.List[string]
        for ($i = $cmds.Count - 1; $i -ge 0; $i--) {
            $entry = $cmds[$i]
            if (-not [string]::IsNullOrWhiteSpace($entry) -and $seen.Add($entry)) {
                $idx = $unique.Count
                $unique.Add($entry)
                $display = $entry.Replace("`r", '').Replace("`n", ' ').Replace("`t", ' ')
                $items.Add("$idx`t$display")
            }
        }
        if ($items.Count -eq 0) { return }
        $queryArg = _goatty_ps_native_arg "--query=$line"
        $selected = $items | & fzf --scheme=history --no-sort "--delimiter=`t" --with-nth=2.. $queryArg
        if (-not [string]::IsNullOrEmpty($selected)) {
            $first = @($selected)[0]
            $tabIdx = $first.IndexOf("`t")
            if ($tabIdx -gt 0) {
                $idx = 0
                if ([int]::TryParse($first.Substring(0, $tabIdx), [ref]$idx) -and $idx -ge 0 -and $idx -lt $unique.Count) {
                    [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $line.Length, $unique[$idx])
                }
            }
        }
        [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
    } -ErrorAction SilentlyContinue

    # Ctrl+T: fuzzy-pick files and insert PowerShell-quoted paths at the cursor.
    Set-PSReadLineKeyHandler -Chord 'Ctrl+t' -BriefDescription 'goatty fzf insert files' -ScriptBlock {
        param($key, $arg)
        $cfg = $global:_goatty_fzf_cfg
        $fzfArgs = New-Object System.Collections.Generic.List[string]
        $fzfArgs.Add('-m')
        if ($cfg.previews -and (-not [string]::IsNullOrEmpty($cfg.bat))) {
            $theme = if ([string]::IsNullOrEmpty($env:BAT_THEME)) { 'ansi' } else { $env:BAT_THEME }
            $fzfArgs.Add((_goatty_ps_native_arg "--preview=`"$($cfg.bat)`" --color=always --style=numbers --line-range=:200 --theme=$theme -- {}"))
        }
        $selected = $null
        if (-not [string]::IsNullOrEmpty($cfg.fd)) {
            $fdArgs = New-Object System.Collections.Generic.List[string]
            $fdArgs.Add('--strip-cwd-prefix')
            if ($cfg.hidden) { $fdArgs.Add('--hidden') }
            $fdArgs.Add('--follow')
            $fdArgs.Add('--exclude')
            $fdArgs.Add('.git')
            $fdArgs.Add('--type')
            $fdArgs.Add('f')
            $selected = & $cfg.fd @fdArgs 2>$null | & fzf @fzfArgs
        } else {
            $walk = if ($cfg.hidden) { 'file,follow,hidden' } else { 'file,follow' }
            $fzfArgs.Add("--walker=$walk")
            $fzfArgs.Add('--walker-skip=.git')
            $selected = & fzf @fzfArgs
        }
        if ($selected) {
            $quoted = @($selected | Where-Object { -not [string]::IsNullOrEmpty($_) } | ForEach-Object {
                $p = [string]$_
                if ($p.StartsWith('-')) { $p = ".\$p" }
                _goatty_ps_quote_arg $p
            })
            if ($quoted.Count -gt 0) {
                [Microsoft.PowerShell.PSConsoleReadLine]::Insert(($quoted -join ' ') + ' ')
            }
        }
        [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
    } -ErrorAction SilentlyContinue

    # Alt+C: fuzzy-pick a directory and cd into it.
    Set-PSReadLineKeyHandler -Chord 'Alt+c' -BriefDescription 'goatty fzf change directory' -ScriptBlock {
        param($key, $arg)
        $cfg = $global:_goatty_fzf_cfg
        $fzfArgs = New-Object System.Collections.Generic.List[string]
        if ($cfg.previews -and (-not [string]::IsNullOrEmpty($cfg.eza))) {
            $fzfArgs.Add((_goatty_ps_native_arg "--preview=`"$($cfg.eza)`" -1 --icons=always --color=always --group-directories-first -- {}"))
        }
        $selected = $null
        if (-not [string]::IsNullOrEmpty($cfg.fd)) {
            $fdArgs = New-Object System.Collections.Generic.List[string]
            $fdArgs.Add('--strip-cwd-prefix')
            if ($cfg.hidden) { $fdArgs.Add('--hidden') }
            $fdArgs.Add('--follow')
            $fdArgs.Add('--exclude')
            $fdArgs.Add('.git')
            $fdArgs.Add('--type')
            $fdArgs.Add('d')
            $selected = & $cfg.fd @fdArgs 2>$null | & fzf @fzfArgs
        } else {
            $walk = if ($cfg.hidden) { 'dir,follow,hidden' } else { 'dir,follow' }
            $fzfArgs.Add("--walker=$walk")
            $fzfArgs.Add('--walker-skip=.git')
            $selected = & fzf @fzfArgs
        }
        if (-not [string]::IsNullOrEmpty($selected)) {
            $dir = @($selected)[0]
            if (-not [string]::IsNullOrEmpty($dir) -and (Test-Path -LiteralPath $dir -PathType Container)) {
                Set-Location -LiteralPath $dir
            }
        }
        [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
    } -ErrorAction SilentlyContinue
}

Remove-Variable -Name _goatty_fzf_hidden, _goatty_fzf_previews, _goatty_fzf_fd, _goatty_fzf_bat, _goatty_fzf_eza -ErrorAction SilentlyContinue
