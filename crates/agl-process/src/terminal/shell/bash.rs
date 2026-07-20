use std::path::Path;

use crate::terminal::history::TerminalHistorySeed;

use super::integration::ShellIntegrationToken;
use super::{HostStartupPolicy, shell_quote};

const MAX_COMMAND_CHARACTERS: usize = 8 * 1024;

pub(super) fn render_startup(
    policy: &HostStartupPolicy,
    seed_path: &Path,
    startup_path: &Path,
    integration_path: &Path,
    token: &ShellIntegrationToken,
) -> String {
    let mut script = String::from(
        "# agentLIBRE managed Bash startup; generated, do not edit\n\
         shopt -s cmdhist lithist\n\
         shopt -u extdebug\n\
         set +a +x +v +T\n\
         set -m\n\
         set -o history\n\
         HISTCONTROL=\n\
         HISTIGNORE=\n\
         HISTTIMEFORMAT=\n\
         unset PROMPT_COMMAND\n\
         trap - DEBUG\n\
         export HISTFILE=/dev/null\n",
    );
    if let HostStartupPolicy::SourceUserRc { path } = policy {
        script.push_str("if [[ -r ");
        script.push_str(&shell_quote(path));
        script.push_str(" ]]; then source ");
        script.push_str(&shell_quote(path));
        script.push_str("; fi\n");
    }
    // Hooks are installed after an optional host rc. Disable tracing/export
    // modes before placing the private token in a non-exported shell variable.
    script.push_str(
        "shopt -s cmdhist lithist\n\
         shopt -u extdebug\n\
         set +a +x +v +T\n\
         set -m\n\
         set -o history\n\
         HISTCONTROL=\n\
         HISTIGNORE=\n\
         HISTTIMEFORMAT=\n\
         unset PROMPT_COMMAND\n\
         trap - DEBUG\n\
         export HISTFILE=/dev/null\n\
         builtin unset -f __agl_write __agl_emit_prompt __agl_emit_started \
           __agl_emit_finished __agl_preexec __agl_precmd 2>/dev/null || :\n\
         builtin unset -v __agl_integration_token __agl_integration_fifo \
           __agl_integration_sequence __agl_integration_active \
           __agl_integration_at_prompt __agl_integration_seen_prompt \
           __agl_integration_guard __agl_ds __agl_dc 2>/dev/null || :\n",
    );
    script.push_str("if [[ -r ");
    script.push_str(&shell_quote(seed_path));
    script.push_str(" ]]; then history -r ");
    script.push_str(&shell_quote(seed_path));
    script.push_str("; fi\n");
    script.push_str("readonly __agl_integration_token=");
    script.push_str(token.expose_to_managed_startup());
    script.push_str("\nreadonly __agl_integration_fifo=");
    script.push_str(&shell_quote(integration_path));
    script.push_str(
        "\n__agl_integration_sequence=0\n\
         __agl_integration_active=0\n\
         __agl_integration_at_prompt=0\n\
         __agl_integration_seen_prompt=0\n\
         __agl_integration_guard=0\n",
    );
    // Remove token-bearing startup material before accepting commands. The
    // files remain private if the admitted core utility is unavailable.
    script.push_str("builtin command rm -f -- ");
    script.push_str(&shell_quote(startup_path));
    script.push(' ');
    script.push_str(&shell_quote(seed_path));
    script.push_str(" 2>/dev/null || :\n");
    script.push_str(
        "__agl_write() {\n\
           local __agl_format=$1 __agl_fd\n\
           shift\n\
           builtin shopt -s varredir_close\n\
           { builtin printf \"$__agl_format\" \"$@\" >&\"$__agl_fd\"; } \
             2>/dev/null {__agl_fd}>\"$__agl_integration_fifo\" || :\n\
         }\n\
         __agl_emit_prompt() {\n\
           local __agl_status=$1 __agl_last=$2 __agl_cwd\n\
           __agl_cwd=$(builtin pwd -P) || __agl_cwd=$PWD\n\
           ((__agl_integration_sequence += 1))\n\
           __agl_write 'AGL1\\0%s\\0%s\\0prompt_ready\\0%s\\0%s\\0' \
             \"$__agl_integration_token\" \"$__agl_integration_sequence\" \
             \"$__agl_cwd\" \"$__agl_last\"\n\
           return \"$__agl_status\"\n\
         }\n\
         __agl_emit_started() {\n\
           local __agl_command=$1 __agl_cwd=$2\n\
           if (( ${#__agl_command} > ",
    );
    script.push_str(&MAX_COMMAND_CHARACTERS.to_string());
    script.push_str(
        " )); then\n\
             __agl_command=${__agl_command:0:",
    );
    script.push_str(&MAX_COMMAND_CHARACTERS.to_string());
    script.push_str(
        "}\n\
           fi\n\
           ((__agl_integration_sequence += 1))\n\
           __agl_write 'AGL1\\0%s\\0%s\\0command_started\\0%s\\0%s\\0' \
             \"$__agl_integration_token\" \"$__agl_integration_sequence\" \
             \"$__agl_command\" \"$__agl_cwd\"\n\
         }\n\
         __agl_emit_finished() {\n\
           local __agl_status=$1 __agl_cwd=$2\n\
           ((__agl_integration_sequence += 1))\n\
           __agl_write 'AGL1\\0%s\\0%s\\0command_finished\\0code\\0%s\\0%s\\0' \
             \"$__agl_integration_token\" \"$__agl_integration_sequence\" \
             \"$__agl_status\" \"$__agl_cwd\"\n\
           return \"$__agl_status\"\n\
         }\n\
         __agl_preexec() {\n\
           local __agl_status=$1 __agl_fallback=$2 __agl_history __agl_command __agl_cwd\n\
           builtin unset -v __agl_ds __agl_dc 2>/dev/null || :\n\
           if [[ $__agl_integration_guard -ne 0 \
              || $__agl_integration_at_prompt -ne 1 \
              || $__agl_fallback == '__agl_precmd' ]]; then\n\
             return \"$__agl_status\"\n\
           fi\n\
           __agl_integration_guard=1\n\
           __agl_integration_at_prompt=0\n\
           __agl_history=$(HISTTIMEFORMAT= builtin history 1) || __agl_history=\n\
           __agl_command=${__agl_history#*[0-9]  }\n\
           [[ -n $__agl_command ]] || __agl_command=$__agl_fallback\n\
           __agl_cwd=$(builtin pwd -P) || __agl_cwd=$PWD\n\
           __agl_integration_active=1\n\
           __agl_emit_started \"$__agl_command\" \"$__agl_cwd\"\n\
           __agl_integration_guard=0\n\
           return \"$__agl_status\"\n\
         }\n\
         __agl_precmd() {\n\
           local __agl_status=$? __agl_last __agl_cwd\n\
           __agl_integration_guard=1\n\
           __agl_cwd=$(builtin pwd -P) || __agl_cwd=$PWD\n\
           if [[ $__agl_integration_active -eq 1 ]]; then\n\
             __agl_emit_finished \"$__agl_status\" \"$__agl_cwd\"\n\
             __agl_integration_active=0\n\
           fi\n\
           if [[ $__agl_integration_seen_prompt -eq 0 ]]; then\n\
             __agl_last=-\n\
             __agl_integration_seen_prompt=1\n\
           else\n\
             __agl_last=$__agl_status\n\
           fi\n\
           __agl_emit_prompt \"$__agl_status\" \"$__agl_last\"\n\
           __agl_integration_at_prompt=1\n\
           __agl_integration_guard=0\n\
           return \"$__agl_status\"\n\
         }\n\
         PROMPT_COMMAND=__agl_precmd\n\
         trap '__agl_ds=$?; __agl_dc=$BASH_COMMAND; builtin set +a +T; \
           builtin shopt -u extdebug; builtin export -n __agl_ds __agl_dc; \
           __agl_preexec \"$__agl_ds\" \"$__agl_dc\"' DEBUG\n",
    );
    script
}

pub(super) fn render_history(seed: &TerminalHistorySeed) -> Vec<u8> {
    let mut rendered = Vec::new();
    for command in seed.commands() {
        rendered.extend_from_slice(command.as_bytes());
        rendered.push(b'\n');
    }
    rendered
}
