#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  scripts/bump-workspace-version.sh [--dry-run] [--message MESSAGE]

Bumps [workspace.package].version to the next vX.Y.Z-alpha.N checkpoint based
on the latest local signed/annotated alpha tag, then:

  1. updates Cargo.lock with cargo generate-lockfile
  2. commits Cargo.toml and Cargo.lock
  3. creates a signed tag for the new version

The script does not fetch tags. Run git fetch --tags first if local tags may be
stale.

Options:
  --dry-run          Print the next version and planned commands, but do not write.
  --message MESSAGE Commit message to use. Default: "Bump workspace version".
  -h, --help         Show this help text.
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

dry_run=0
commit_message="Bump workspace version"

while (($#)); do
  case "$1" in
    --dry-run)
      dry_run=1
      ;;
    --message)
      shift
      [[ $# -gt 0 ]] || die "--message requires a value"
      commit_message="$1"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
  shift
done

repo_root="$(git rev-parse --show-toplevel)"
cd "$repo_root"

cargo_toml="$repo_root/Cargo.toml"
[[ -f "$cargo_toml" ]] || die "Cargo.toml not found at repo root"

workspace_version() {
  awk '
    /^[[:space:]]*\[workspace\.package\][[:space:]]*$/ {
      in_workspace_package = 1
      next
    }
    /^[[:space:]]*\[/ {
      if (in_workspace_package) {
        exit
      }
    }
    in_workspace_package && /^[[:space:]]*version[[:space:]]*=/ {
      line = $0
      sub(/^[[:space:]]*version[[:space:]]*=[[:space:]]*"/, "", line)
      sub(/".*$/, "", line)
      print line
      exit
    }
  ' "$cargo_toml"
}

latest_alpha_tag() {
  git tag --list |
    grep -E '^v[0-9]+\.[0-9]+\.[0-9]+-alpha\.[0-9]+$' |
    sort -V |
    tail -n 1
}

next_alpha_version() {
  local base_version="$1"
  if [[ ! "$base_version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)-alpha\.([0-9]+)$ ]]; then
    die "unsupported checkpoint version: $base_version"
  fi

  local major="${BASH_REMATCH[1]}"
  local minor="${BASH_REMATCH[2]}"
  local patch="${BASH_REMATCH[3]}"
  local alpha="${BASH_REMATCH[4]}"

  printf "%s.%s.%s-alpha.%s\n" "$major" "$minor" "$patch" "$((alpha + 1))"
}

update_workspace_version() {
  local version="$1"
  local tmp
  tmp="$(mktemp "$repo_root/Cargo.toml.tmp.XXXXXX")"

  if ! awk -v new_version="$version" '
    BEGIN {
      in_workspace_package = 0
      changed = 0
    }
    /^[[:space:]]*\[workspace\.package\][[:space:]]*$/ {
      in_workspace_package = 1
      print
      next
    }
    /^[[:space:]]*\[/ {
      if (in_workspace_package) {
        in_workspace_package = 0
      }
    }
    in_workspace_package && /^[[:space:]]*version[[:space:]]*=/ {
      match($0, /^[[:space:]]*/)
      indent = substr($0, RSTART, RLENGTH)
      print indent "version = \"" new_version "\""
      changed = 1
      next
    }
    {
      print
    }
    END {
      if (!changed) {
        exit 42
      }
    }
  ' "$cargo_toml" > "$tmp"; then
    rm -f "$tmp"
    die "failed to update [workspace.package].version in Cargo.toml"
  fi

  mv "$tmp" "$cargo_toml"
}

create_signed_tag() {
  local tag="$1"
  if ! git tag -s "$tag" -m "$tag"; then
    cat >&2 <<EOF
error: failed to create signed tag $tag
The version bump commit may already exist, but the checkpoint tag was not
created. After fixing signing, rerun this script or run:
  git tag -s $tag -m "$tag"
EOF
    exit 1
  fi
}

current_workspace_version="$(workspace_version)"
[[ -n "$current_workspace_version" ]] || die "could not read [workspace.package].version"

latest_tag="$(latest_alpha_tag || true)"
[[ -n "$latest_tag" ]] || die "no vX.Y.Z-alpha.N tag found; create the first checkpoint tag manually"

base_version="${latest_tag#v}"
next_version="$(next_alpha_version "$base_version")"
next_tag="v$next_version"

if git rev-parse -q --verify "refs/tags/$next_tag" >/dev/null; then
  die "tag already exists: $next_tag"
fi

if [[ "$current_workspace_version" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)-alpha\.([0-9]+)$ ]]; then
  workspace_base="${BASH_REMATCH[1]}.${BASH_REMATCH[2]}.${BASH_REMATCH[3]}"
  workspace_alpha="${BASH_REMATCH[4]}"
  next_base="${next_version%-alpha.*}"
  next_alpha="${next_version##*.}"
  if [[ "$workspace_base" == "$next_base" && "$workspace_alpha" -gt "$next_alpha" ]]; then
    die "workspace version $current_workspace_version is ahead of next tag-derived version $next_version"
  fi
fi

cat <<EOF
latest_tag=$latest_tag
current_workspace_version=$current_workspace_version
next_workspace_version=$next_version
next_tag=$next_tag
commit_message=$commit_message
EOF

if ((dry_run)); then
  cat <<EOF
dry_run=true
planned:
  update Cargo.toml workspace version to $next_version
  cargo generate-lockfile
  git add Cargo.toml Cargo.lock
  git commit -m "$commit_message"
  git tag -s $next_tag -m "$next_tag"
EOF
  exit 0
fi

[[ -z "$(git status --porcelain)" ]] || die "working tree is dirty; commit or stash changes before bumping version"

if [[ "$current_workspace_version" == "$next_version" ]]; then
  echo "workspace version is already $next_version; skipping commit and creating $next_tag on HEAD"
  create_signed_tag "$next_tag"
  echo "created $next_tag"
  exit 0
fi

update_workspace_version "$next_version"
cargo generate-lockfile

git add Cargo.toml Cargo.lock

if git diff --cached --quiet; then
  die "version bump produced no staged changes"
fi

git commit -m "$commit_message"
create_signed_tag "$next_tag"

echo "created $next_tag"
