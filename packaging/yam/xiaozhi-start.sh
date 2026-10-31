#!/bin/sh
# Launcher for the YAM service: the client resolves its runtime config
# (xiaozhi_config.json) relative to the working directory, so run from a
# writable state dir. The compile-time defaults (config.toml baked in) are
# used on the first start; override in the generated xiaozhi_config.json.
set -eu

STATE_DIR=${XIAOZHI_STATE_DIR:-/data/opt/xiaozhi}
PKG_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

mkdir -p "$STATE_DIR"
cd "$STATE_DIR"

exec "$PKG_DIR/xiaozhi_linux_rs" "$@"
