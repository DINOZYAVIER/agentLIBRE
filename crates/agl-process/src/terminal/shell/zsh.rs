use std::path::Path;

use crate::terminal::history::TerminalHistorySeed;

use super::integration::ShellIntegrationToken;
use super::{HostStartupPolicy, shell_quote};

pub(super) fn render_startup(
    policy: &HostStartupPolicy,
    seed_path: &Path,
    startup_path: &Path,
    event_path: &Path,
    control_path: &Path,
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
         unfunction precmd preexec __agl_write __agl_read_control __agl_consume_controls \
           __agl_wait_boundary __agl_wait_prompt __agl_emit_prompt __agl_emit_started \
           __agl_emit_finished __agl_preexec __agl_precmd 2>/dev/null || :\n\
         unset __agl_integration_token __agl_event_fifo __agl_control_fifo \
           __agl_integration_sequence __agl_command_sequence __agl_integration_active \
           __agl_active_transaction __agl_integration_seen_prompt __agl_integration_enabled \
           __agl_armed_transaction __agl_control 2>/dev/null || :\n",
    );
    script.push_str("if [[ -r ");
    script.push_str(&shell_quote(seed_path));
    script.push_str(" ]]; then fc -R ");
    script.push_str(&shell_quote(seed_path));
    script.push_str("; fi\n");
    script.push_str("typeset -gr __agl_integration_token=");
    script.push_str(token.expose_to_managed_startup());
    script.push_str("\ntypeset -gr __agl_event_fifo=");
    script.push_str(&shell_quote(event_path));
    script.push_str("\ntypeset -gr __agl_control_fifo=");
    script.push_str(&shell_quote(control_path));
    script.push_str(
        "\ntypeset -gi __agl_integration_sequence=0\n\
         typeset -gi __agl_command_sequence=0\n\
         typeset -gi __agl_integration_active=0\n\
         typeset -g __agl_active_transaction=-\n\
         typeset -gi __agl_integration_seen_prompt=0\n\
         typeset -gi __agl_integration_enabled=1\n",
    );
    script.push_str("command rm -f -- ");
    script.push_str(&shell_quote(startup_path));
    script.push(' ');
    script.push_str(&shell_quote(seed_path));
    script.push_str(" 2>/dev/null || :\n");
    script.push_str(
        r#"__agl_write() {
  (( __agl_integration_enabled )) || return 1
  local __agl_format=$1
  integer __agl_fd=0
  shift
  exec {__agl_fd}<>"$__agl_event_fifo" || return 1
  builtin printf "$__agl_format" "$@" >&$__agl_fd
  local __agl_result=$?
  exec {__agl_fd}>&-
  return $__agl_result
}
__agl_read_control() {
  local __agl_timeout=$1 __agl_field
  integer __agl_fd=0 __agl_index=0
  typeset -ga __agl_control
  __agl_control=()
  exec {__agl_fd}<>"$__agl_control_fifo" || return 1
  for __agl_index in 1 2 3 4 5; do
    IFS= builtin read -r -d $'\0' -t "$__agl_timeout" __agl_field <&$__agl_fd || {
      exec {__agl_fd}>&-
      return 1
    }
    __agl_control+=("$__agl_field")
  done
  exec {__agl_fd}>&-
  [[ ${__agl_control[1]} == AGL2 && ${__agl_control[2]} == "$__agl_integration_token" ]] || return 2
  return 0
}
__agl_consume_controls() {
  local __agl_expected=$1
  integer __agl_result=0
  typeset -g __agl_armed_transaction=-
  while true; do
    __agl_read_control 0.002
    __agl_result=$?
    (( __agl_result == 1 )) && break
    (( __agl_result == 0 )) || return 1
    case ${__agl_control[3]} in
      arm_typed_command)
        [[ ${__agl_control[5]} == "$__agl_expected" && ${__agl_control[4]} != - ]] || return 1
        __agl_armed_transaction=${__agl_control[4]}
        ;;
      disarm_typed_command)
        if [[ ${__agl_control[4]} == "$__agl_armed_transaction" ]]; then
          __agl_armed_transaction=-
        fi
        ;;
      *) return 1 ;;
    esac
  done
  return 0
}
__agl_wait_boundary() {
  local __agl_transaction=$1 __agl_boundary=$2
  __agl_read_control 1 || return 1
  [[ ${__agl_control[3]} == command_boundary_ack \
     && ${__agl_control[4]} == "$__agl_transaction" \
     && ${__agl_control[5]} == "$__agl_boundary" ]]
}
__agl_wait_prompt() {
  local __agl_event_sequence=$1
  __agl_read_control 1 || return 1
  [[ ${__agl_control[3]} == prompt_ready_ack \
     && ${__agl_control[4]} == "$__agl_event_sequence" \
     && ( ${__agl_control[5]} == - || ${__agl_control[5]} == <-> ) ]] || return 1
  [[ ${__agl_control[5]} == - ]] && return 2
  return 0
}
__agl_emit_prompt() {
  local __agl_last=$1 __agl_cwd=${PWD:A} __agl_attempt __agl_result
  for __agl_attempt in 1 2; do
    (( ++__agl_integration_sequence ))
    __agl_write 'AGL2\0%s\0%s\0prompt_ready\0%s\0%s\0-\0' \
      "$__agl_integration_token" "$__agl_integration_sequence" "$__agl_cwd" "$__agl_last" || return 1
    if __agl_wait_prompt "$__agl_integration_sequence"; then
      return 0
    else
      __agl_result=$?
    fi
    (( __agl_result == 2 )) || return 1
  done
  return 0
}
__agl_emit_started() {
  local __agl_transaction=$1 __agl_command=$2 __agl_cwd=$3
  (( ++__agl_integration_sequence ))
  __agl_write 'AGL2\0%s\0%s\0command_started\0%s\0%s\0%s\0' \
    "$__agl_integration_token" "$__agl_integration_sequence" "$__agl_transaction" \
    "$__agl_command" "$__agl_cwd"
}
__agl_emit_finished() {
  local __agl_status=$1 __agl_transaction=$2 __agl_cwd=$3
  (( ++__agl_integration_sequence ))
  __agl_write 'AGL2\0%s\0%s\0command_finished\0%s\0code\0%s\0%s\0' \
    "$__agl_integration_token" "$__agl_integration_sequence" "$__agl_transaction" \
    "$__agl_status" "$__agl_cwd"
}
__agl_preexec() {
  local __agl_command=${1:-$3} __agl_cwd=${PWD:A}
  (( ++__agl_command_sequence ))
  if (( __agl_integration_enabled )); then
    __agl_consume_controls "$__agl_command_sequence" || {
      builtin kill -KILL "$$"
      return 1
    }
    __agl_active_transaction=$__agl_armed_transaction
    __agl_integration_active=1
    __agl_emit_started "$__agl_active_transaction" "$__agl_command" "$__agl_cwd" || {
      [[ $__agl_active_transaction == - ]] || builtin kill -KILL "$$"
      __agl_integration_enabled=0
      return 1
    }
    if [[ $__agl_active_transaction != - ]] \
       && ! __agl_wait_boundary "$__agl_active_transaction" started; then
      builtin kill -KILL "$$"
      return 1
    fi
  fi
}
__agl_precmd() {
  local __agl_status=$? __agl_last __agl_cwd=${PWD:A}
  if (( __agl_integration_enabled && __agl_integration_active )); then
    __agl_emit_finished $__agl_status "$__agl_active_transaction" "$__agl_cwd" || __agl_integration_enabled=0
    if (( __agl_integration_enabled )) && [[ $__agl_active_transaction != - ]] \
       && ! __agl_wait_boundary "$__agl_active_transaction" finished; then
      __agl_integration_enabled=0
    fi
    __agl_integration_active=0
    __agl_active_transaction=-
  fi
  if (( ! __agl_integration_seen_prompt )); then
    __agl_last=-
    __agl_integration_seen_prompt=1
  else
    __agl_last=$__agl_status
  fi
  if (( __agl_integration_enabled )) && ! __agl_emit_prompt "$__agl_last"; then
    __agl_integration_enabled=0
  fi
  return $__agl_status
}
typeset -ga precmd_functions preexec_functions
precmd_functions=(__agl_precmd)
preexec_functions=(__agl_preexec)
"#,
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
