#!/usr/bin/env bash

agl_systemd_validate_unit_name() {
  local unit="$1"
  if [[ "$unit" == */* || "$unit" == *$'\n'* || -z "$unit" ]]; then
    echo "--unit must be a unit name, not a path: $unit" >&2
    exit 2
  fi
}

agl_systemd_validate_absolute_path() {
  local option="$1"
  local value="$2"
  if [[ "$value" != /* ]]; then
    echo "$option must be absolute: $value" >&2
    exit 2
  fi
  if [[ "$value" == *$'\n'* ]]; then
    echo "$option must not contain newlines" >&2
    exit 2
  fi
}

agl_systemd_validate_absolute_vars() {
  local value_name
  local value
  for value_name in "$@"; do
    value="${!value_name}"
    agl_systemd_validate_absolute_path "--${value_name//_/-}" "$value"
  done
}

agl_systemd_validate_nonempty_no_newline() {
  local option="$1"
  local value="$2"
  if [[ "$value" == *$'\n'* || -z "$value" ]]; then
    echo "$option must be non-empty and must not contain newlines" >&2
    exit 2
  fi
}

agl_systemd_require_dir() {
  local dry_run="$1"
  local path="$2"
  local label="$3"
  if [[ "$dry_run" -eq 0 && ! -d "$path" ]]; then
    echo "$label does not exist: $path" >&2
    exit 1
  fi
}

agl_systemd_require_executable() {
  local dry_run="$1"
  local path="$2"
  if [[ "$dry_run" -eq 0 && ! -x "$path" ]]; then
    echo "binary does not exist or is not executable: $path" >&2
    exit 1
  fi
}

agl_systemd_require_file() {
  local dry_run="$1"
  local path="$2"
  local label="$3"
  if [[ "$dry_run" -eq 0 && ! -f "$path" ]]; then
    echo "$label does not exist: $path" >&2
    exit 1
  fi
}

agl_systemd_quote() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '"%s"' "$value"
}

agl_systemd_install_user_unit() {
  local unit_dir="$1"
  local unit="$2"
  local unit_content="$3"
  local enable="$4"
  local restart="$5"

  mkdir -p "$unit_dir"
  local unit_file="$unit_dir/$unit"
  local tmp_file
  tmp_file="$(mktemp "$unit_dir/.${unit}.XXXXXX")"
  printf '%s' "$unit_content" > "$tmp_file"
  chmod 0644 "$tmp_file"
  mv "$tmp_file" "$unit_file"

  systemctl --user daemon-reload
  systemctl --user reset-failed "$unit" || true

  if [[ "$enable" -eq 1 ]]; then
    systemctl --user enable "$unit"
  fi

  if [[ "$restart" -eq 1 ]]; then
    systemctl --user restart "$unit"
  fi

  systemctl --user --no-pager show "$unit" -p UnitFileState -p ActiveState -p ExecStart
}

agl_systemd_print_or_install_user_unit() {
  local dry_run="$1"
  local unit_dir="$2"
  local unit="$3"
  local unit_content="$4"
  local enable="$5"
  local restart="$6"

  if [[ "$dry_run" -eq 1 ]]; then
    printf '\n%s' "$unit_content"
    return 0
  fi

  agl_systemd_install_user_unit "$unit_dir" "$unit" "$unit_content" "$enable" "$restart"
}
