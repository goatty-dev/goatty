# goatty's built-in autosuggestions plugin for PowerShell.
# Uses PSReadLine's native inline prediction on PowerShell 7.2+ (PSReadLine 2.2+)
# and degrades gracefully on Windows PowerShell 5.1 (PSReadLine 2.0).

$_goatty_psrl = Get-Module -Name PSReadLine
if ($_goatty_psrl -and ($_goatty_psrl.Version -ge [Version]'2.2.0')) {
    $_goatty_pred_src = if ($goatty_settings['strategy'] -eq 'history') { 'History' } else { 'HistoryAndPlugin' }
    $_goatty_esc = [char]27
    try {
        Set-PSReadLineOption -PredictionSource $_goatty_pred_src -PredictionViewStyle InlineView -Colors @{ InlinePrediction = "$_goatty_esc[90m" } -ErrorAction Stop
        $global:_goatty_state['autosuggestions'] = "ok`tPSReadLine $($_goatty_psrl.Version) ($_goatty_pred_src)"
    } catch {
        try {
            Set-PSReadLineOption -PredictionSource History -PredictionViewStyle InlineView -Colors @{ InlinePrediction = "$_goatty_esc[90m" } -ErrorAction Stop
            $global:_goatty_state['autosuggestions'] = "ok`tPSReadLine $($_goatty_psrl.Version) (History)"
        } catch {
            $global:_goatty_state['autosuggestions'] = "ok`tPSReadLine $($_goatty_psrl.Version) (predictions unavailable)"
        }
    }
    Remove-Variable -Name _goatty_pred_src, _goatty_esc -ErrorAction SilentlyContinue
} elseif ($_goatty_psrl) {
    $global:_goatty_state['autosuggestions'] = "ok`tneeds PSReadLine 2.2+ (found $($_goatty_psrl.Version); install PowerShell 7)"
} else {
    $global:_goatty_state['autosuggestions'] = 'missing'
}
Remove-Variable -Name _goatty_psrl -ErrorAction SilentlyContinue
