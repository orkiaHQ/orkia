#!/usr/bin/env bash
# Copyright 2026 Orkia — Elastic-2.0
#
# can create unprivileged user namespaces on Ubuntu 24.04+ (where
# kernel.apparmor_restrict_unprivileged_userns=1 blocks it). This is the prod
# fix — preferred over the `sysctl …=0` dev hatch, because it grants userns to
# ONLY the orkia-cage binary, not the whole system.
#
# Usage:  sudo ./install.sh [/path/to/orkia-cage]
#   (defaults to `command -v orkia-cage`)
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
bin="${1:-$(command -v orkia-cage 2>/dev/null || true)}"
if [ -z "$bin" ]; then
  echo "error: orkia-cage not found on PATH — pass its path: sudo $0 /path/to/orkia-cage" >&2
  exit 2
fi
bin="$(readlink -f "$bin")"
if [ ! -x "$bin" ]; then
  echo "error: $bin is not an executable" >&2
  exit 2
fi
if [ "$(id -u)" -ne 0 ]; then
  echo "error: must run as root (writes /etc/apparmor.d, runs apparmor_parser)" >&2
  exit 2
fi

dest=/etc/apparmor.d/orkia-cage
sed "s|@EXEC_PATH@|$bin|g" "$here/orkia-cage.profile.in" > "$dest"
echo "wrote $dest  (exec path: $bin)"

apparmor_parser -r "$dest"
echo "loaded profile 'orkia-cage'"

if command -v aa-status >/dev/null 2>&1 && aa-status 2>/dev/null | grep -q 'orkia-cage'; then
  echo "active: orkia-cage profile is loaded"
else
  echo "loaded (aa-status not available or profile not listed — verify manually)"
fi
echo "Done. orkia-cage may now create unprivileged user namespaces without the sysctl hatch."
