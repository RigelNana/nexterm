# NexTerm shell integration for PowerShell (minimal)
# Wraps the prompt function with OSC 133;A (prompt start) and 133;B
# (command start). The terminal uses the next 133;A to implicitly
# finalize the previous block, so 133;D is not needed.
#
# IMPORTANT: No PSConsoleHostReadLine wrapping, no Write-Host calls,
# no side-effect-inducing code. This avoids double-prompt issues that
# cause incorrect block splitting on command errors.

$script:__nexterm_orig_prompt = $function:global:prompt
$function:global:prompt = {
    $p = if ($script:__nexterm_orig_prompt) {
        & $script:__nexterm_orig_prompt
    } else {
        "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) "
    }
    "$([char]0x1b)]133;A$([char]0x07)$p$([char]0x1b)]133;B$([char]0x07)"
}
