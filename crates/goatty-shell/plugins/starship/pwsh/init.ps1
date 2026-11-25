# goatty's built-in Starship plugin for PowerShell (PS 7 and Windows PowerShell 5.1).
# goatty generates starship.toml from the prompt settings (starship-plain.toml without
# Nerd Font icons for other terminals); your own config wins when prefer_user_config is on.

$_goatty_user_starship = Join-Path (Join-Path $HOME '.config') 'starship.toml'
if (($goatty_settings['prefer_user_config'] -ne 'true') -or ([string]::IsNullOrEmpty($env:STARSHIP_CONFIG) -and (-not (Test-Path -LiteralPath $_goatty_user_starship)))) {
    $env:STARSHIP_CONFIG = Join-Path $global:_goatty_dir $global:_goatty_starship_toml
}
Remove-Variable -Name _goatty_user_starship -ErrorAction SilentlyContinue

# Keep Starship quiet when a module times out under load unless the user overrides it.
if ([string]::IsNullOrEmpty($env:STARSHIP_LOG)) {
    $env:STARSHIP_LOG = 'error'
}

$_goatty_ss_cfg = if ([string]::IsNullOrEmpty($env:STARSHIP_CONFIG)) { Join-Path (Join-Path $HOME '.config') 'starship.toml' } else { $env:STARSHIP_CONFIG }
$_goatty_ss_key = 'none'
if (Test-Path -LiteralPath $_goatty_ss_cfg -PathType Leaf) {
    $fi = Get-Item -LiteralPath $_goatty_ss_cfg -ErrorAction SilentlyContinue
    if ($fi) {
        $_goatty_ss_key = "$($fi.FullName):$($fi.Length):$($fi.LastWriteTimeUtc.Ticks)"
    }
}

$_goatty_ss_file = _goatty_cached_init -Name 'starship' -Extra $_goatty_ss_key -Command 'starship' -Arguments @('init', 'powershell', '--print-full-init')
if (-not [string]::IsNullOrEmpty($_goatty_ss_file) -and (Test-Path -LiteralPath $_goatty_ss_file)) {
    . $_goatty_ss_file
} elseif (-not [string]::IsNullOrEmpty($global:_goatty_init_code)) {
    Invoke-Expression $global:_goatty_init_code
    $global:_goatty_init_code = $null
}

# Transient prompt: shrink older prompts to a single character once a command runs.
# Handled by goatty's shared Enter handler and prompt wrapper (init.ps1 / integration.ps1)
# so expansions, transient redraw, and OSC 133;C cooperate without clobbering Enter.
if ($global:_goatty_prompt_transient) {
    $global:_goatty_transient_active = $true
}

Remove-Variable -Name _goatty_ss_cfg, _goatty_ss_key, _goatty_ss_file -ErrorAction SilentlyContinue
