# goatty expansions (#89, shell.expansions): plain words that turn into a
# command while you type. `| first 20 lines` + Space becomes `| head -n 20`.
#
# The rules are data (expansions.zsh, generated from the plugins'
# [[expansion]] tables and host.json's expansions): for each, a regular
# expression for the end of the text left of the cursor, and what to put
# there instead. This file is the same for everyone; no plugin code runs.
#
# * Space expands the rule that matches what's left of the cursor. Only
#   rules whose last word was just typed are tried (a hash lookup), so a
#   Space costs next to nothing.
# * Enter on a line that ends in a rule expands it and stops, so you see
#   the command before it runs. Enter again runs it.
# * Backspace right after an expansion brings back what you typed.
# * Never inside quotes, and never in pasted text (a paste doesn't go
#   through these widgets).

typeset -ga _goatty_exp_re _goatty_exp_to _goatty_exp_say
typeset -gA _goatty_exp_last _goatty_exp_last2
builtin source -- "$_goatty_dir/expansions.zsh" || return 1
typeset -g _goatty_exp_undo='' _goatty_exp_after=''

# Whether the end of $1 is inside quotes.
_goatty_exp_quoted() {
  local s=$1 q='' c
  integer i
  for (( i = 1; i <= $#s; i++ )); do
    c=$s[i]
    if [[ -z $q ]]; then
      case $c in
        \\) (( i++ )) ;;
        [\'\"]) q=$c ;;
      esac
    elif [[ $q == '"' && $c == \\ ]]; then
      (( i++ ))
    elif [[ $c == $q ]]; then
      q=''
    fi
  done
  [[ -n $q ]]
}

# Replace the end of LBUFFER by the first rule that matches it. Status 1
# when none does.
_goatty_exp_apply() {
  emulate -L zsh -o extended_glob
  local left=$LBUFFER
  local w1=${left##*[[:space:]]}
  [[ -n $w1 ]] || return 1
  local before=${left[1,-$#w1-1]}
  before=${before%%[[:space:]]##}
  local w2=${before##*[[:space:]]}
  local ids=${_goatty_exp_last[$w1]-}
  [[ -n $w2 ]] && ids+=" ${_goatty_exp_last2[$w2]-}"
  [[ -n ${ids// /} ]] || return 1
  local sep=$'\x01' out i
  integer k
  local -a parts match mbegin mend
  local MATCH MBEGIN MEND
  for i in ${=ids}; do
    [[ $left =~ ${_goatty_exp_re[i]} ]] || continue
    _goatty_exp_quoted "$left" && return 1
    parts=("${(@ps:$sep:)_goatty_exp_to[i]}")
    out=''
    for (( k = 1; k <= $#parts; k++ )); do
      if (( k % 2 )); then
        out+=$parts[k]
      else
        out+=$match[$parts[k]]
      fi
    done
    _goatty_exp_undo=$UNDO_CHANGE_NO
    LBUFFER=${left[1,MBEGIN-1]}$match[1]$out
    _goatty_exp_after=$LBUFFER
    zle -M "${MATCH#$match[1]}  →  $out    (Backspace: undo)"
    return 0
  done
  return 1
}

# Right after a paste: what was pasted stays as it is.
_goatty_exp_pasted() { [[ $LASTWIDGET == *bracketed-paste* ]] }

goatty-expand-space() {
  _goatty_exp_pasted || [[ -n $RBUFFER && $RBUFFER != [[:space:]]* ]] || _goatty_exp_apply
  zle self-insert
}

goatty-expand-accept() {
  # Shown, not run: Enter again runs it.
  if [[ -z $RBUFFER ]] && ! _goatty_exp_pasted && _goatty_exp_apply; then
    return 0
  fi
  zle accept-line
}

goatty-expand-backspace() {
  if [[ -n $_goatty_exp_undo && $LASTWIDGET == goatty-expand-(space|accept) \
     && ( $LBUFFER == "$_goatty_exp_after" || $LBUFFER == "$_goatty_exp_after " ) ]]; then
    zle undo $_goatty_exp_undo
    _goatty_exp_undo=''
    zle -M ''
    return 0
  fi
  _goatty_exp_undo=''
  zle backward-delete-char
}

# Nothing to expand: leave the keys alone.
(( $#_goatty_exp_re )) || return 0
zle -N goatty-expand-space
zle -N goatty-expand-accept
zle -N goatty-expand-backspace
() {
  local km
  for km in emacs viins; do
    bindkey -M $km ' ' goatty-expand-space
    bindkey -M $km '^M' goatty-expand-accept
    # Only plain Backspace (^?): ^H is Ctrl+Backspace (backward-kill-word).
    bindkey -M $km '^?' goatty-expand-backspace
  done
}
