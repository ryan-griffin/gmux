#!/usr/bin/env bash
# Create a container-local account so home lookup and cache ownership work.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${GMUX_CI_UID:?Set GMUX_CI_UID to the checkout owner's uid}"
: "${GMUX_CI_GID:?Set GMUX_CI_GID to the checkout owner's gid}"
[[ "$GMUX_CI_UID" =~ ^[0-9]+$ && "$GMUX_CI_GID" =~ ^[0-9]+$ ]]

if ! getent group "$GMUX_CI_GID" >/dev/null; then
    groupadd --gid "$GMUX_CI_GID" gmux-ci
fi
if ! getent passwd "$GMUX_CI_UID" >/dev/null; then
    useradd --no-log-init --create-home --uid "$GMUX_CI_UID" \
        --gid "$GMUX_CI_GID" --shell /bin/bash gmux-ci
fi
ci_user="$(getent passwd "$GMUX_CI_UID" | cut -d: -f1)"
exec runuser --user "$ci_user" -- bash ci/check.sh
