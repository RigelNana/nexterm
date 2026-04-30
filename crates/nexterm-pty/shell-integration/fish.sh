# NexTerm shell integration for Fish
# Emits OSC 133 semantic prompt markers for block detection.

function __nexterm_prompt_start --on-event fish_prompt
    # OSC 133;A — prompt start
    printf '\e]133;A\a'
end

function __nexterm_command_start --on-event fish_preexec
    # OSC 133;C — command output start
    printf '\e]133;C\a'
end

function __nexterm_command_end --on-event fish_postexec
    # OSC 133;D — command finished with exit code
    printf '\e]133;D;%s\a' $status
end

# Wrap existing fish_prompt to append OSC 133;B
functions -c fish_prompt __nexterm_original_fish_prompt 2>/dev/null
function fish_prompt
    __nexterm_original_fish_prompt
    printf '\e]133;B\a'
end
