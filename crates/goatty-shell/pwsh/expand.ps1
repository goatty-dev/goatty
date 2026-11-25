# goatty expansions for PowerShell (#89, #278): plain words that turn into a
# command while you type (`| first 20 lines` + Space -> `| head -n 20`).
#
# Rules live as quoted data in expansions.ps1; this static script runs the
# matching and PSReadLine Space/Enter/Backspace handlers.

$global:_goatty_exp_re = @()
$global:_goatty_exp_to = @()
$global:_goatty_exp_say = @()
$global:_goatty_exp_last = New-Object System.Collections.Hashtable([System.StringComparer]::Ordinal)
$global:_goatty_exp_last2 = New-Object System.Collections.Hashtable([System.StringComparer]::Ordinal)
$global:_goatty_exp_active = $false

$_goatty_exp_data = Join-Path $_goatty_dir 'expansions.ps1'
if (-not (Test-Path -LiteralPath $_goatty_exp_data)) { return }
. $_goatty_exp_data

$global:_goatty_exp_undo_before = $null
$global:_goatty_exp_undo_after = $null

function global:_goatty_exp_quoted([string]$s) {
    $q = ''
    for ($i = 0; $i -lt $s.Length; $i++) {
        $c = $s[$i]
        if ($q -eq '') {
            if ($c -eq '`') { $i++ }
            elseif ($c -eq "'" -or $c -eq '"') { $q = [string]$c }
        } elseif ($q -eq '"' -and $c -eq '`') {
            $i++
        } elseif ([string]$c -eq $q) {
            if ($q -eq "'" -and ($i + 1) -lt $s.Length -and $s[$i + 1] -eq "'") {
                $i++
            } else {
                $q = ''
            }
        }
    }
    return ($q -ne '')
}

function global:_goatty_exp_apply {
    $line = $null; $cursor = $null
    [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
    if ($cursor -le 0) { return $false }
    $left = $line.Substring(0, $cursor)
    $m1 = [regex]::Match($left, '(\S+)$')
    if (-not $m1.Success) { return $false }
    $w1 = $m1.Groups[1].Value
    $before = $left.Substring(0, $m1.Index).TrimEnd()
    $m2 = [regex]::Match($before, '(\S+)$')
    $w2 = if ($m2.Success) { $m2.Groups[1].Value } else { '' }

    $ids = New-Object System.Collections.Generic.List[int]
    if ($global:_goatty_exp_last.ContainsKey($w1)) {
        foreach ($idx in $global:_goatty_exp_last[$w1]) { $ids.Add([int]$idx) }
    }
    if (-not [string]::IsNullOrEmpty($w2) -and $global:_goatty_exp_last2.ContainsKey($w2)) {
        foreach ($idx in $global:_goatty_exp_last2[$w2]) { $ids.Add([int]$idx) }
    }
    if ($ids.Count -eq 0) { return $false }

    $sep = [char]1
    foreach ($i in $ids) {
        $m = [regex]::Match($left, $global:_goatty_exp_re[$i])
        if (-not $m.Success) { continue }
        if (_goatty_exp_quoted $left) { return $false }
        $parts = $global:_goatty_exp_to[$i].Split($sep)
        $out = ''
        for ($k = 0; $k -lt $parts.Length; $k++) {
            if (($k % 2) -eq 0) {
                $out += $parts[$k]
            } else {
                $g = [int]$parts[$k]
                $out += $m.Groups[$g].Value
            }
        }
        $prefix = $m.Groups[1].Value
        $replacement = $prefix + $out
        $global:_goatty_exp_undo_before = $left
        $newLeft = $left.Substring(0, $m.Index) + $replacement
        $global:_goatty_exp_undo_after = $newLeft
        [Microsoft.PowerShell.PSConsoleReadLine]::Replace($m.Index, $m.Length, $replacement)
        return $true
    }
    return $false
}

if ($global:_goatty_exp_re.Count -eq 0 -or -not $global:_goatty_psreadline) {
    return
}

try {
    Set-PSReadLineKeyHandler -Key Spacebar -ScriptBlock {
        $line = $null; $cursor = $null
        [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
        $right = $line.Substring($cursor)
        if ([string]::IsNullOrEmpty($right) -or [char]::IsWhiteSpace($right[0])) {
            [void](_goatty_exp_apply)
        }
        [Microsoft.PowerShell.PSConsoleReadLine]::Insert(' ')
    } -ErrorAction SilentlyContinue

    # Enter is handled by the shared `_goatty_enter_handler` in init.ps1 so
    # expansions, transient prompt redraw, and OSC 133;C cooperate cleanly.
    $global:_goatty_exp_active = $true

    Set-PSReadLineKeyHandler -Key Backspace -ScriptBlock {
        $line = $null; $cursor = $null
        [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
        if ($null -ne $global:_goatty_exp_undo_before -and $null -ne $global:_goatty_exp_undo_after) {
            $left = $line.Substring(0, $cursor)
            if ($left -eq $global:_goatty_exp_undo_after -or $left -eq ($global:_goatty_exp_undo_after + ' ')) {
                $restore = $global:_goatty_exp_undo_before
                $global:_goatty_exp_undo_before = $null
                $global:_goatty_exp_undo_after = $null
                [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $cursor, $restore)
                return
            }
        }
        $global:_goatty_exp_undo_before = $null
        $global:_goatty_exp_undo_after = $null
        [Microsoft.PowerShell.PSConsoleReadLine]::BackwardDeleteChar()
    } -ErrorAction SilentlyContinue
} catch {}
