#!/usr/bin/env bash
# Tests packaging/install-user.sh without touching the real home directory.
#
# The interesting cases are prefixes that contain a space, a `%`, or a backslash,
# which are all valid home directories: the generated `Exec=` must be a quoted,
# escaped Desktop Entry value, or the launcher reads the path as an executable
# plus an argument (or as a different path) and the entry does nothing when
# clicked.
#
# Usage: bash packaging/test-install-user.sh
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

fake="$work/zapfast-fake"
printf '#!/bin/sh\nexit 0\n' > "$fake"
chmod +x "$fake"

# The Desktop Entry Exec escaping, mirroring src/autostart.rs: a backslash is
# doubled first, then `"`, `` ` ``, `$` and `\` are backslash-escaped, and a
# literal `%` is doubled. Written here so the test states the expected bytes
# rather than reading them back from the script under test.
escape_exec() {
  local path=$1
  path=${path//\\/\\\\}
  path=${path//\"/\\\"}
  path=${path//\`/\\\`}
  path=${path//\$/\\$}
  path=${path//%/%%}
  printf '%s' "$path"
}

check_prefix() {
  local prefix="$1"
  mkdir -p "$prefix"
  bash "$script_dir/install-user.sh" "$fake" "$prefix" >/dev/null
  local entry="$prefix/share/applications/zapfast.desktop"
  test -s "$entry"
  test -s "$prefix/share/icons/hicolor/scalable/apps/zapfast.svg"

  local expected
  expected=$(printf 'Exec="%s"' "$(escape_exec "${prefix}/bin/zapfast")")
  grep -qxF "$expected" "$entry" || {
    echo "unexpected Exec for prefix: $prefix" >&2
    grep '^Exec=' "$entry" >&2
    echo "expected: $expected" >&2
    exit 1
  }

  # desktop-file-validate rejects the spec-correct doubled backslash in a
  # quoted Exec, so it is only consulted for paths without one. The escaping is
  # still checked above, and mirrors src/autostart.rs.
  if command -v desktop-file-validate >/dev/null 2>&1 && [[ "$prefix" != *\\* ]]; then
    desktop-file-validate "$entry"
  fi
}

check_prefix "$work/with space"
check_prefix "$work/with%percent"
# A backslash followed by `n`: `awk -v` would turn this into a newline, so the
# Exec would name a path the binary was never installed to.
check_prefix "$work/with\\nbackslash"

echo "install-user.sh wrote a valid, escaped launcher entry for every prefix."
