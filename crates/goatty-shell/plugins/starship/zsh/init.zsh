# goatty's built-in Starship plugin. goatty generates starship.toml from the
# prompt settings; your own config wins when prefer_user_config is on.
if [[ $goatty_settings[prefer_user_config] != true ]] || [[ -z $STARSHIP_CONFIG && ! -e ${XDG_CONFIG_HOME:-$HOME/.config}/starship.toml ]]; then
  builtin export STARSHIP_CONFIG=$_goatty_dir/starship.toml
fi
# Starship logs a warning into the terminal whenever a module's command
# outlives command_timeout (git or rustc right after login, under load).
# The prompt just leaves that module out; keep it quiet unless you ask.
builtin export STARSHIP_LOG=${STARSHIP_LOG:-error}
eval "$(starship init zsh)"
