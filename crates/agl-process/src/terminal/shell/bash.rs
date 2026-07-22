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
         if [[ $- == *i* ]]; then\n\
           builtin bind 'set enable-bracketed-paste on' 2>/dev/null || :\n\
         fi\n\
         builtin unset -f __agl_write __agl_read_control __agl_consume_controls \
           __agl_wait_boundary __agl_wait_prompt __agl_emit_prompt \
           __agl_emit_started __agl_emit_finished __agl_preexec __agl_precmd \
           2>/dev/null || :\n\
         builtin unset -v __agl_integration_token __agl_event_fifo __agl_control_fifo \
           __agl_integration_sequence __agl_command_sequence __agl_integration_active \
           __agl_active_transaction __agl_integration_at_prompt \
           __agl_integration_seen_prompt __agl_integration_guard __agl_integration_enabled \
           __agl_armed_transaction __agl_control __agl_ds __agl_dc 2>/dev/null || :\n",
    );
    script.push_str("if [[ -r ");
    script.push_str(&shell_quote(seed_path));
    script.push_str(" ]]; then history -r ");
    script.push_str(&shell_quote(seed_path));
    script.push_str("; fi\n");
    script.push_str("readonly __agl_integration_token=");
    script.push_str(token.expose_to_managed_startup());
    script.push_str("\nreadonly __agl_event_fifo=");
    script.push_str(&shell_quote(event_path));
    script.push_str("\nreadonly __agl_control_fifo=");
    script.push_str(&shell_quote(control_path));
    script.push_str(
        "\n__agl_integration_sequence=0\n\
         __agl_command_sequence=0\n\
         __agl_integration_active=0\n\
         __agl_active_transaction=-\n\
         __agl_integration_at_prompt=0\n\
         __agl_integration_seen_prompt=0\n\
         __agl_integration_guard=0\n\
         __agl_integration_enabled=1\n",
    );
    script.push_str("builtin command rm -f -- ");
    script.push_str(&shell_quote(startup_path));
    script.push(' ');
    script.push_str(&shell_quote(seed_path));
    script.push_str(" 2>/dev/null || :\n");
    script.push_str(
        r#"__agl_write() {
  [[ $__agl_integration_enabled -eq 1 ]] || return 1
  local __agl_format=$1 __agl_fd
  shift
  builtin shopt -s varredir_close
  exec {__agl_fd}<>"$__agl_event_fifo" || return 1
  builtin printf "$__agl_format" "$@" >&"$__agl_fd"
  local __agl_result=$?
  exec {__agl_fd}>&-
  return "$__agl_result"
}
__agl_read_control() {
  local __agl_timeout=$1 __agl_fd __agl_field __agl_index
  __agl_control=()
  builtin shopt -s varredir_close
  exec {__agl_fd}<>"$__agl_control_fifo" || return 1
  for ((__agl_index=0; __agl_index<5; __agl_index++)); do
    IFS= builtin read -r -d '' -t "$__agl_timeout" __agl_field <&"$__agl_fd" || {
      exec {__agl_fd}>&-
      return 1
    }
    __agl_control+=("$__agl_field")
  done
  exec {__agl_fd}>&-
  [[ ${__agl_control[0]} == AGL2 && ${__agl_control[1]} == "$__agl_integration_token" ]] || return 2
  return 0
}
__agl_consume_controls() {
  local __agl_expected=$1 __agl_result
  __agl_armed_transaction=-
  while :; do
    __agl_read_control 0.002
    __agl_result=$?
    [[ $__agl_result -eq 1 ]] && break
    [[ $__agl_result -eq 0 ]] || return 1
    case ${__agl_control[2]} in
      arm_typed_command)
        [[ ${__agl_control[4]} == "$__agl_expected" && ${__agl_control[3]} != - ]] || return 1
        __agl_armed_transaction=${__agl_control[3]}
        ;;
      disarm_typed_command)
        if [[ ${__agl_control[3]} == "$__agl_armed_transaction" ]]; then
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
  [[ ${__agl_control[2]} == command_boundary_ack \
     && ${__agl_control[3]} == "$__agl_transaction" \
     && ${__agl_control[4]} == "$__agl_boundary" ]]
}
__agl_wait_prompt() {
  local __agl_event_sequence=$1
  __agl_read_control 1 || return 1
  [[ ${__agl_control[2]} == prompt_ready_ack \
     && ${__agl_control[3]} == "$__agl_event_sequence" \
     && ( ${__agl_control[4]} == - || ${__agl_control[4]} =~ ^[0-9]+$ ) ]] || return 1
  [[ ${__agl_control[4]} == - ]] && return 2
  return 0
}
__agl_emit_prompt() {
  local __agl_status=$1 __agl_last=$2 __agl_cwd __agl_attempt __agl_result
  __agl_cwd=$(builtin pwd -P) || __agl_cwd=$PWD
  for __agl_attempt in 1 2; do
    ((__agl_integration_sequence += 1))
    __agl_write 'AGL2\0%s\0%s\0prompt_ready\0%s\0%s\0-\0' \
      "$__agl_integration_token" "$__agl_integration_sequence" "$__agl_cwd" "$__agl_last" || return 1
    if __agl_wait_prompt "$__agl_integration_sequence"; then
      return 0
    else
      __agl_result=$?
    fi
    [[ $__agl_result -eq 2 ]] || return 1
  done
  return 0
}
__agl_emit_started() {
  local __agl_transaction=$1 __agl_command=$2 __agl_cwd=$3
  ((__agl_integration_sequence += 1))
  __agl_write 'AGL2\0%s\0%s\0command_started\0%s\0%s\0%s\0' \
    "$__agl_integration_token" "$__agl_integration_sequence" "$__agl_transaction" \
    "$__agl_command" "$__agl_cwd"
}
__agl_emit_finished() {
  local __agl_status=$1 __agl_transaction=$2 __agl_cwd=$3
  ((__agl_integration_sequence += 1))
  __agl_write 'AGL2\0%s\0%s\0command_finished\0%s\0code\0%s\0%s\0' \
    "$__agl_integration_token" "$__agl_integration_sequence" "$__agl_transaction" \
    "$__agl_status" "$__agl_cwd"
}
__agl_preexec() {
  local __agl_status=$1 __agl_fallback=$2 __agl_history __agl_command __agl_cwd
  builtin unset -v __agl_ds __agl_dc 2>/dev/null || :
  if [[ $__agl_integration_guard -ne 0 \
     || $__agl_integration_at_prompt -ne 1 \
     || $__agl_fallback == '__agl_precmd' ]]; then
    return "$__agl_status"
  fi
  __agl_integration_guard=1
  __agl_integration_at_prompt=0
  __agl_history=$(HISTTIMEFORMAT= builtin history 1) || __agl_history=
  __agl_command=${__agl_history#*[0-9]  }
  [[ -n $__agl_command ]] || __agl_command=$__agl_fallback
  __agl_cwd=$(builtin pwd -P) || __agl_cwd=$PWD
  ((__agl_command_sequence += 1))
  if [[ $__agl_integration_enabled -eq 1 ]]; then
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
  __agl_integration_guard=0
  return "$__agl_status"
}
__agl_precmd() {
  local __agl_status=$? __agl_last __agl_cwd
  __agl_integration_guard=1
  __agl_cwd=$(builtin pwd -P) || __agl_cwd=$PWD
  if [[ $__agl_integration_enabled -eq 1 && $__agl_integration_active -eq 1 ]]; then
    __agl_emit_finished "$__agl_status" "$__agl_active_transaction" "$__agl_cwd" || __agl_integration_enabled=0
    if [[ $__agl_integration_enabled -eq 1 && $__agl_active_transaction != - ]] \
       && ! __agl_wait_boundary "$__agl_active_transaction" finished; then
      __agl_integration_enabled=0
    fi
    __agl_integration_active=0
    __agl_active_transaction=-
  fi
  if [[ $__agl_integration_seen_prompt -eq 0 ]]; then
    __agl_last=-
    __agl_integration_seen_prompt=1
  else
    __agl_last=$__agl_status
  fi
  if [[ $__agl_integration_enabled -eq 1 ]] \
     && ! __agl_emit_prompt "$__agl_status" "$__agl_last"; then
    __agl_integration_enabled=0
  fi
  __agl_integration_at_prompt=1
  __agl_integration_guard=0
  return "$__agl_status"
}
PROMPT_COMMAND=__agl_precmd
trap '__agl_ds=$?; __agl_dc=$BASH_COMMAND; builtin set +a +T; \
  builtin shopt -u extdebug; builtin export -n __agl_ds __agl_dc; \
  __agl_preexec "$__agl_ds" "$__agl_dc"' DEBUG
"#,
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
