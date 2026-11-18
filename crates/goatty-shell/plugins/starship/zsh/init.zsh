# goatty's built-in Starship plugin. goatty generates starship.toml from the
# prompt settings; your own config wins when prefer_user_config is on.
if [[ ${goatty_settings[prefer_user_config]:-} != true ]] || [[ -z ${STARSHIP_CONFIG:-} && ! -e $HOME/.config/starship.toml ]]; then
  builtin export STARSHIP_CONFIG=$_goatty_dir/starship.toml
fi
# Starship logs a warning into the terminal whenever a module's command
# outlives command_timeout (git or rustc right after login, under load).
# The prompt just leaves that module out; keep it quiet unless you ask.
builtin export STARSHIP_LOG=${STARSHIP_LOG:-error}
() {
  builtin emulate -L zsh
  local cfg=${STARSHIP_CONFIG:-$HOME/.config/starship.toml}
  local -A cfg_st
  typeset -g _goatty_starship_key=none
  typeset -g _goatty_starship_name=starship
  if [[ -e $cfg ]] && (( $+builtins[zstat] )) && builtin zstat -F '%s.%N' -H cfg_st -- "${cfg:A}" 2>/dev/null; then
    _goatty_starship_key="${cfg:A}:${cfg_st[device]}:${cfg_st[inode]}:${cfg_st[size]}:${cfg_st[mtime]}:${cfg_st[ctime]}"
  fi
  if [[ ${TERM:-} == dumb ]]; then
    _goatty_starship_key+=":dumb"
    _goatty_starship_name=starship-dumb
  fi
}
_goatty_starship_post() {
  builtin emulate -L zsh
  local bin=$1 out=$2
  if [[ ${TERM:-} != dumb && $out == *'PROMPT2="$('* ]]; then
    local p2
    if p2=$(ARGV0=${3:-$bin} STARSHIP_SHELL=zsh "$bin" prompt --continuation 2>/dev/null); then
      local -a lines=("${(@f)out}")
      local i
      for i in {1..${#lines}}; do
        if [[ ${lines[$i]} == PROMPT2=\"\$\(*\)\" ]]; then
          lines[$i]="PROMPT2=${(qq)p2}"
        fi
      done
      out=${(F)lines}
    fi
  fi
  REPLY=$out
}
_goatty_rc=0
_goatty_cached_init -p _goatty_starship_post "$_goatty_starship_name" "$_goatty_starship_key" starship init zsh --print-full-init || _goatty_rc=$?
if (( _goatty_rc == 0 )) && { [[ ! -r $REPLY ]] || { ! builtin source -- "$REPLY" && [[ ! -r $REPLY ]]; }; }; then
  _goatty_rc=0
  _goatty_cached_init -n -p _goatty_starship_post "$_goatty_starship_name" "$_goatty_starship_key" starship init zsh --print-full-init || _goatty_rc=$?
fi
if (( _goatty_rc == 2 )); then
  builtin eval "$_goatty_init_code"
fi
builtin unset _goatty_starship_name _goatty_starship_key _goatty_init_code _goatty_rc
builtin unfunction _goatty_starship_post 2>/dev/null
