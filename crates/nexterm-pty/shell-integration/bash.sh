# NexTerm shell integration for Bash
# Emits OSC 133 semantic prompt markers for block detection.

__nexterm_precmd() {
    local exit_code="$?"
    # OSC 133;D — previous command finished
    if [ -n "$__nexterm_had_command" ]; then
        printf '\e]133;D;%s\a' "$exit_code"
    fi
    __nexterm_had_command=1
    # OSC 133;A — prompt start
    printf '\e]133;A\a'
}

__nexterm_preexec() {
    # OSC 133;C — command output start
    printf '\e]133;C\a'
}

# Wrap PS1 with OSC 133;B at the end (after prompt, before user input)
__nexterm_setup() {
    # Install precmd via PROMPT_COMMAND
    if [[ "$PROMPT_COMMAND" != *"__nexterm_precmd"* ]]; then
        PROMPT_COMMAND="__nexterm_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
    fi

    # Install preexec via DEBUG trap
    # Only fire once per command (not for each pipeline stage)
    __nexterm_preexec_fired=0
    trap '__nexterm_debug_trap' DEBUG

    # Append OSC 133;B to PS1
    if [[ "$PS1" != *'133;B'* ]]; then
        PS1="${PS1}"$'\[\e]133;B\a\]'
    fi
}

__nexterm_debug_trap() {
    # Skip if inside PROMPT_COMMAND or already fired
    if [[ -n "$COMP_LINE" ]] || [[ "$BASH_COMMAND" == "$PROMPT_COMMAND" ]]; then
        return
    fi
    if [[ "$__nexterm_preexec_fired" == "0" ]]; then
        __nexterm_preexec_fired=1
        __nexterm_preexec
    fi
}

# Re-arm preexec after each prompt
__nexterm_precmd_rearm() {
    __nexterm_preexec_fired=0
}

# Prepend rearm to PROMPT_COMMAND
__nexterm_setup
PROMPT_COMMAND="__nexterm_precmd_rearm;$PROMPT_COMMAND"
