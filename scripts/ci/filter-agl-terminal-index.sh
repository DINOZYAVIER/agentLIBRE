#!/usr/bin/env bash
set -euo pipefail

# Executed by git filter-branch inside the disposable extraction clone. Keep
# only paths selected by AGL-151/H03; the source repository is never mutated.
git ls-files -z |
  perl -0ne '
    print unless m{\Acrates/(?:
      agl-exec|
      agl-pty|
      agl-terminal|
      agl-terminal-protocol|
      agl-terminal-client|
      agl-process-launcher|
      agl-terminald
    )(?:/|\z)}x
  ' |
  xargs -0 -r git rm -r -q --cached --ignore-unmatch --
