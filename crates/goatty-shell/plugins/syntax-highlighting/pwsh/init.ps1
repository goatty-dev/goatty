# goatty's built-in syntax highlighting plugin for PowerShell (PS 7 and PS 5.1).
# Configures PSReadLine's native syntax highlighting using the terminal's 16 ANSI
# colors so command-line highlighting follows goatty's active color theme.

$_goatty_psrl = Get-Module -Name PSReadLine
if ($_goatty_psrl) {
    $_goatty_esc = [char]27
    try {
        Set-PSReadLineOption -Colors @{
            Command   = "$_goatty_esc[32m"
            Parameter = "$_goatty_esc[36m"
            Operator  = "$_goatty_esc[35m"
            Variable  = "$_goatty_esc[33m"
            String    = "$_goatty_esc[33m"
            Number    = "$_goatty_esc[35m"
            Type      = "$_goatty_esc[34m"
            Comment   = "$_goatty_esc[90m"
            Keyword   = "$_goatty_esc[35m"
            Error     = "$_goatty_esc[31m"
            Member    = "$_goatty_esc[34m"
            Default   = "$_goatty_esc[0m"
        } -ErrorAction SilentlyContinue
        $global:_goatty_state['syntax-highlighting'] = "ok`tPSReadLine $($_goatty_psrl.Version)"
    } catch {
        $global:_goatty_state['syntax-highlighting'] = "ok`tPSReadLine $($_goatty_psrl.Version)"
    }
    Remove-Variable -Name _goatty_esc -ErrorAction SilentlyContinue
} else {
    $global:_goatty_state['syntax-highlighting'] = 'missing'
}
Remove-Variable -Name _goatty_psrl -ErrorAction SilentlyContinue
