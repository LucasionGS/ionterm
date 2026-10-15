# ionterm shell integration for zsh: marks prompts and commands (OSC 133)
# and reports the working directory (OSC 7). Changes nothing else.
if [[ -z $__ionterm_integrated ]]; then
  typeset -g __ionterm_integrated=1

  # First precmd: the command that just ran finished (the terminal ignores
  # this when none did).
  __ionterm_precmd() {
    local ret=$?
    print -n "\e]133;D;$ret\a\e]7;file://$HOST$PWD\a"
    return $ret
  }
  # Last precmd, right before the prompt is drawn: it starts here.
  __ionterm_prompt() { print -n "\e]133;A\a" }
  # A command line was accepted: its output starts here. The command
  # goes along (control characters blanked) for "finished" notifications.
  __ionterm_preexec() { print -nr -- $'\e]133;C;cmdline='"${1//[[:cntrl:]]/ }"$'\a' }

  autoload -Uz add-zsh-hook
  add-zsh-hook preexec __ionterm_preexec
  precmd_functions=(__ionterm_precmd $precmd_functions __ionterm_prompt)
fi
