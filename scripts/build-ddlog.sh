#!/bin/sh
# Trusted build driver: source.dl output-executable. DDlog v1.2.3 emits Rust.
set -eu
source_path=$1
output_path=$2
: "${DDLOG_HOME:?Set DDLOG_HOME to the DDlog distribution}"
cd "$(dirname "$source_path")"
"$DDLOG_HOME/bin/ddlog" -i "$(basename "$source_path")"
cd program_ddlog
# Optional offline packaging overrides; these are operator configuration.
if [ -n "${DDLOG_CARGO_CONFIG:-}" ]; then
    cp "$DDLOG_CARGO_CONFIG" .cargo/config.toml
fi
if [ -n "${DDLOG_CARGO_LOCK:-}" ]; then
    cp "$DDLOG_CARGO_LOCK" Cargo.lock
fi
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_INCREMENTAL=0
if [ "${DDLOG_OFFLINE:-0}" = 1 ]; then
    "${DDLOG_CARGO:-cargo}" build --offline --locked --bin program_cli
else
    "${DDLOG_CARGO:-cargo}" build --bin program_cli
fi
cp "${CARGO_TARGET_DIR:-target}/debug/program_cli" "$output_path"
