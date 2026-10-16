# ionterm shell integration for bash: marks prompts and commands (OSC 133)
# and reports the working directory (OSC 7). Sourced after the user's own
# startup files; changes nothing else.
if [[ -z $__ionterm_integrated ]]; then
  __ionterm_integrated=1
  __ionterm_host=${HOSTNAME:-$(uname -n)}

  # Runs first: the command that just ran finished (the terminal ignores
  # this when none did). Keeps $? for whatever runs after it.
  __ionterm_prompt_start() {
    local status=$?
    printf '\e]133;D;%s\a\e]7;file://%s%s\a' "$status" "$__ionterm_host" "$PWD"
    return "$status"
  }
  # Runs last, right before the prompt is drawn: it starts here. The end
  # of PS1 marks where the command line starts (prompt themes that set PS1
  # every time get it back each time).
  __ionterm_prompt_end() {
    local status=$?
    [[ $PS1 == *'133;B'* ]] || PS1="$PS1"'\[\e]133;B\a\]'
    printf '\e]133;A\a'
    return "$status"
  }

  if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == "declare -a"* ]]; then
    PROMPT_COMMAND=(__ionterm_prompt_start "${PROMPT_COMMAND[@]}" __ionterm_prompt_end)
  else
    PROMPT_COMMAND="__ionterm_prompt_start${PROMPT_COMMAND:+;$PROMPT_COMMAND};__ionterm_prompt_end"
  fi
  # Printed once a command line is read, before it runs: output starts here.
  PS0="${PS0}"'\e]133;C\a'
fi
