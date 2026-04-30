# NexTerm shell integration for Zsh
# Emits OSC 133 semantic prompt markers for block detection.

__nexterm_precmd() {
    local exit_code="$?"
    # OSC 133;D — previous command finished
    if [[ -n "$__nexterm_had_command" ]]; then
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

# Append OSC 133;B to the prompt (after prompt text, before user input)
__nexterm_setup() {
    autoload -Uz add-zsh-hook
    add-zsh-hook precmd __nexterm_precmd
    add-zsh-hook preexec __nexterm_preexec

    # Append OSC 133;B to PS1 if not already present
    if [[ "$PS1" != *'133;B'* ]]; then
        PS1="${PS1}"$'%{\e]133;B\a%}'
    fi
}

__nexterm_setup
