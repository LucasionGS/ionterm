# ionterm shell integration for fish 3 (fish 4 marks prompts by itself):
# marks prompts and commands (OSC 133). Changes nothing else.
if not set -q __ionterm_integrated
    set -g __ionterm_integrated 1
    function __ionterm_mark_prompt --on-event fish_prompt
        printf '\e]133;A\a'
    end
    function __ionterm_mark_start --on-event fish_preexec
        printf '\e]133;C\a'
    end
    function __ionterm_mark_end --on-event fish_postexec
        printf '\e]133;D;%s\a' $status
    end
end
