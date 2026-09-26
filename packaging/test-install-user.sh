#!/usr/bin/env bash
# Tests packaging/install-user.sh without touching the real home directory.
#
# The interesting case is a prefix that contains a space or a `%`, which is a
# perfectly valid home directory: the generated `Exec=` must be a quoted,
# escaped Desktop Entry value, or the launcher reads the path as an executable
# plus an argument and the entry does nothing when clicked.
#
# Usage: bash packaging/test-install-user.sh
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

fake="$work/zapfast-fake"
printf '#!/bin/sh\nexit 0\n' > "$fake"
chmod +x "$fake"

check_prefix() {
  local prefix="$1"
  mkdir -p "$prefix"
  bash "$script_dir/install-user.sh" "$fake" "$prefix" >/dev/null
  local entry="$prefix/share/applications/zapfast.desktop"
  test -s "$entry"
  test -s "$prefix/share/icons/hicolor/scalable/apps/zapfast.svg"

  # The installed binary is quoted, and a literal `%` in the path is doubled,
  # exactly as src/autostart.rs escapes the tray entry's Exec.
  local expected
  expected=$(printf 'Exec="%s"' "${prefix}/bin/zapfast")
  expected=${expected//%/%%}
  grep -qxF "$expected" "$entry" || {
    echo "unexpected Exec for prefix: $prefix" >&2
    grep '^Exec=' "$entry" >&2
    echo "expected: $expected" >&2
    exit 1
  }

  if command -v desktop-file-validate >/dev/null 2>&1; then
    desktop-file-validate "$entry"
  fi
}

check_prefix "$work/with space"
check_prefix "$work/with%percent"

echo "install-user.sh wrote a valid, escaped launcher entry for both prefixes."
