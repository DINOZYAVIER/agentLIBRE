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
        "# agentLIBRE managed Zsh startup; generated, do not edit\n\
         setopt APPEND_HISTORY EXTENDED_HISTORY MONITOR\n\
         unset precmd_functions preexec_functions\n\
         export HISTFILE=/dev/null\n",
    );
    if let HostStartupPolicy::SourceUserRc { path } = policy {
        script.push_str("if [[ -r ");
        script.push_str(&shell_quote(path));
        script.push_str(" ]]; then source ");
        script.push_str(&shell_quote(path));
        script.push_str("; fi\n");
    }
    script.push_str(
        "unsetopt ALLEXPORT XTRACE VERBOSE\n\
         setopt APPEND_HISTORY EXTENDED_HISTORY MONITOR\n\
         export HISTFILE=/dev/null\n\
         unset precmd_functions preexec_functions\n\
         unfunction precmd preexec __agl_write __agl_emit_prompt \
           __agl_emit_started __agl_emit_finished __agl_preexec __agl_precmd \
           2>/dev/null || :\n\
         unset __agl_integration_token __agl_integration_fifo \
           __agl_integration_sequence __agl_integration_active \
           __agl_integration_seen_prompt 2>/dev/null || :\n",
    );
    script.push_str("if [[ -r ");
    script.push_str(&shell_quote(seed_path));
    script.push_str(" ]]; then fc -R ");
    script.push_str(&shell_quote(seed_path));
    script.push_str("; fi\n");
    script.push_str("typeset -gr __agl_integration_token=");
    script.push_str(token.expose_to_managed_startup());
    script.push_str("\ntypeset -gr __agl_integration_fifo=");
    script.push_str(&shell_quote(integration_path));
    script.push_str(
        "\ntypeset -gi __agl_integration_sequence=0\n\
         typeset -gi __agl_integration_active=0\n\
         typeset -gi __agl_integration_seen_prompt=0\n",
    );
    script.push_str("command rm -f -- ");
    script.push_str(&shell_quote(startup_path));
    script.push(' ');
    script.push_str(&shell_quote(seed_path));
    script.push_str(" 2>/dev/null || :\n");
    script.push_str(
        "__agl_write() {\n\
           local __agl_format=$1\n\
           integer __agl_fd=0\n\
           shift\n\
           {\n\
             {\n\
               exec {__agl_fd}>\"$__agl_integration_fifo\" || return 0\n\
               builtin printf \"$__agl_format\" \"$@\" >&$__agl_fd\n\
             } always {\n\
               if (( __agl_fd >= 10 )); then\n\
                 exec {__agl_fd}>&-\n\
               fi\n\
             }\n\
           } 2>/dev/null\n\
           return 0\n\
         }\n\
         __agl_emit_prompt() {\n\
           local __agl_status=$1 __agl_last=$2 __agl_cwd=${PWD:A}\n\
           (( ++__agl_integration_sequence ))\n\
           __agl_write 'AGL1\\0%s\\0%s\\0prompt_ready\\0%s\\0%s\\0' \
             \"$__agl_integration_token\" \"$__agl_integration_sequence\" \
             \"$__agl_cwd\" \"$__agl_last\"\n\
           return $__agl_status\n\
         }\n\
         __agl_emit_started() {\n\
           local __agl_command=$1 __agl_cwd=$2\n\
           if (( ${#__agl_command} > ",
    );
    script.push_str(&MAX_COMMAND_CHARACTERS.to_string());
    script.push_str(
        " )); then\n\
             __agl_command=${__agl_command[1,",
    );
    script.push_str(&MAX_COMMAND_CHARACTERS.to_string());
    script.push_str(
        "]}\n\
           fi\n\
           (( ++__agl_integration_sequence ))\n\
           __agl_write 'AGL1\\0%s\\0%s\\0command_started\\0%s\\0%s\\0' \
             \"$__agl_integration_token\" \"$__agl_integration_sequence\" \
             \"$__agl_command\" \"$__agl_cwd\"\n\
         }\n\
         __agl_emit_finished() {\n\
           local __agl_status=$1 __agl_cwd=$2\n\
           (( ++__agl_integration_sequence ))\n\
           __agl_write 'AGL1\\0%s\\0%s\\0command_finished\\0code\\0%s\\0%s\\0' \
             \"$__agl_integration_token\" \"$__agl_integration_sequence\" \
             \"$__agl_status\" \"$__agl_cwd\"\n\
           return $__agl_status\n\
         }\n\
         __agl_preexec() {\n\
           local __agl_command=${1:-$3} __agl_cwd=${PWD:A}\n\
           __agl_integration_active=1\n\
           __agl_emit_started \"$__agl_command\" \"$__agl_cwd\"\n\
         }\n\
         __agl_precmd() {\n\
           local __agl_status=$? __agl_last __agl_cwd=${PWD:A}\n\
           if (( __agl_integration_active )); then\n\
             __agl_emit_finished $__agl_status \"$__agl_cwd\"\n\
             __agl_integration_active=0\n\
           fi\n\
           if (( ! __agl_integration_seen_prompt )); then\n\
             __agl_last=-\n\
             __agl_integration_seen_prompt=1\n\
           else\n\
             __agl_last=$__agl_status\n\
           fi\n\
           __agl_emit_prompt $__agl_status \"$__agl_last\"\n\
           return $__agl_status\n\
         }\n\
         typeset -ga precmd_functions preexec_functions\n\
         precmd_functions=(__agl_precmd)\n\
         preexec_functions=(__agl_preexec)\n",
    );
    script
}

pub(super) fn render_history(seed: &TerminalHistorySeed) -> Vec<u8> {
    let mut rendered = Vec::new();
    for command in seed.commands() {
        rendered.extend_from_slice(b": 0:0;");
        let mut lines = command.split('\n').peekable();
        while let Some(line) = lines.next() {
            rendered.extend_from_slice(line.as_bytes());
            if lines.peek().is_some() {
                rendered.extend_from_slice(b"\\\n");
            }
        }
        rendered.push(b'\n');
    }
    rendered
}
