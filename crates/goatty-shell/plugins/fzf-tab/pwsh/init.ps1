# goatty's built-in fzf-tab plugin for PowerShell (PS 7 and Windows PowerShell 5.1).
# Replaces Tab completion with PowerShell's native CommandCompletion::CompleteInput
# piped into fzf.exe, falling back to PSReadLine MenuComplete if fzf is unavailable.

$_goatty_ft_previews = ($goatty_settings['previews'] -eq 'true')
$_goatty_ft_bat = $null
$_goatty_ft_eza = $null
if ($_goatty_ft_previews) {
    $ezaCmd = Get-Command -Name 'eza' -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($ezaCmd -and $ezaCmd.Source) {
        $_goatty_ft_eza = $ezaCmd.Source
        $global:_goatty_state['helper:eza'] = "ok`t$($ezaCmd.Source)"
    } else {
        $global:_goatty_state['helper:eza'] = 'missing'
    }
    foreach ($c in @('bat', 'batcat')) {
        $batCmd = Get-Command -Name $c -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
        if ($batCmd -and $batCmd.Source) {
            $_goatty_ft_bat = $batCmd.Source
            $global:_goatty_state['helper:bat'] = "ok`t$($batCmd.Source)"
            break
        }
    }
    if (-not $_goatty_ft_bat) {
        $global:_goatty_state['helper:bat'] = 'missing'
    }
}

$global:_goatty_fzftab_cfg = @{
    previews = $_goatty_ft_previews
    bat      = $_goatty_ft_bat
    eza      = $_goatty_ft_eza
}

if ($global:_goatty_psreadline) {
    Set-PSReadLineKeyHandler -Key Tab -BriefDescription 'goatty fzf-tab completion' -ScriptBlock {
        param($key, $arg)
        if (-not (Get-Command -Name 'fzf' -CommandType Application -ErrorAction SilentlyContinue)) {
            [Microsoft.PowerShell.PSConsoleReadLine]::MenuComplete()
            return
        }
        $line = $null
        $cursor = $null
        [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
        try {
            $comp = [System.Management.Automation.CommandCompletion]::CompleteInput($line, $cursor, $null)
        } catch {
            [Microsoft.PowerShell.PSConsoleReadLine]::MenuComplete()
            return
        }
        if (-not $comp -or $comp.CompletionMatches.Count -eq 0) {
            return
        }
        if ($comp.CompletionMatches.Count -eq 1) {
            [Microsoft.PowerShell.PSConsoleReadLine]::Replace($comp.ReplacementIndex, $comp.ReplacementLength, $comp.CompletionMatches[0].CompletionText)
            return
        }

        $cfg = $global:_goatty_fzftab_cfg
        $entries = New-Object System.Collections.Generic.List[string]
        $allFiles = $true
        $allDirs = $true
        for ($i = 0; $i -lt $comp.CompletionMatches.Count; $i++) {
            $m = $comp.CompletionMatches[$i]
            $label = if ([string]::IsNullOrEmpty($m.ListItemText)) { $m.CompletionText } else { $m.ListItemText }
            $label = $label.Replace("`t", ' ').Replace("`r", '').Replace("`n", ' ')
            $raw = $m.CompletionText.Trim("'", '"')
            $entries.Add("$i`t$label`t$raw")
            if ($m.ResultType -ne [System.Management.Automation.CompletionResultType]::ProviderItem) { $allFiles = $false }
            if ($m.ResultType -ne [System.Management.Automation.CompletionResultType]::ProviderContainer) { $allDirs = $false }
        }

        $fzfArgs = New-Object System.Collections.Generic.List[string]
        $fzfArgs.Add("--delimiter=`t")
        $fzfArgs.Add('--with-nth=2')
        if ($cfg.previews) {
            if ($allDirs -and (-not [string]::IsNullOrEmpty($cfg.eza))) {
                $fzfArgs.Add("--preview=`"$($cfg.eza)`" -1 --icons=always --color=always --group-directories-first -- {3}")
            } elseif ($allFiles -and (-not [string]::IsNullOrEmpty($cfg.bat))) {
                $theme = if ([string]::IsNullOrEmpty($env:BAT_THEME)) { 'ansi' } else { $env:BAT_THEME }
                $fzfArgs.Add("--preview=`"$($cfg.bat)`" --color=always --style=numbers --line-range=:200 --theme=$theme -- {3}")
            }
        }

        $selected = $entries | & fzf @fzfArgs
        if (-not [string]::IsNullOrEmpty($selected)) {
            $first = @($selected)[0]
            $tabIdx = $first.IndexOf("`t")
            if ($tabIdx -gt 0) {
                $idx = 0
                if ([int]::TryParse($first.Substring(0, $tabIdx), [ref]$idx) -and $idx -ge 0 -and $idx -lt $comp.CompletionMatches.Count) {
                    [Microsoft.PowerShell.PSConsoleReadLine]::Replace($comp.ReplacementIndex, $comp.ReplacementLength, $comp.CompletionMatches[$idx].CompletionText)
                }
            }
        }
        [Microsoft.PowerShell.PSConsoleReadLine]::InvokePrompt()
    } -ErrorAction SilentlyContinue
}

$global:_goatty_state['fzf-tab'] = "ok`tCommandCompletion + fzf"
Remove-Variable -Name _goatty_ft_previews, _goatty_ft_bat, _goatty_ft_eza -ErrorAction SilentlyContinue
