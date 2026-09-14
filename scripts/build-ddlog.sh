#!/bin/sh
# Trusted build driver: source.dl output-executable. DDlog v1.2.3 emits Rust.
set -eu
runtime_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
source_path=$1
output_path=$2
: "${DDLOG_HOME:?Set DDLOG_HOME to the DDlog distribution}"
cd "$(dirname "$source_path")"
"$DDLOG_HOME/bin/ddlog" -i "$(basename "$source_path")"
cd program_ddlog
python3 "$runtime_root/scripts/install-observer.py" .
# Optional offline packaging overrides; these are operator configuration.
if [ -n "${DDLOG_CARGO_CONFIG:-}" ]; then
    cp "$DDLOG_CARGO_CONFIG" .cargo/config.toml
fi
if [ -n "${DDLOG_CARGO_LOCK:-}" ]; then
    # An explicit lock wins, but a program that imports lemmalog_star adds the
    # types__lemmalog_star workspace member; a lock without it cannot build
    # --locked, so refuse here with the cause instead of failing inside cargo.
    if [ -d types/lemmalog_star ] && ! grep -q '^name = "types__lemmalog_star"$' "$DDLOG_CARGO_LOCK"; then
        echo "build-ddlog.sh: DDLOG_CARGO_LOCK=$DDLOG_CARGO_LOCK does not lock types__lemmalog_star, which this program's lemmalog_star import requires; unset DDLOG_CARGO_LOCK and set DDLOG_LOCK_DIR to a directory holding star.Cargo.lock (for example $runtime_root/native)" >&2
        exit 1
    fi
    cp "$DDLOG_CARGO_LOCK" Cargo.lock
elif [ -n "${DDLOG_LOCK_DIR:-}" ]; then
    if [ -d types/lemmalog_star ]; then
        cp "$DDLOG_LOCK_DIR/star.Cargo.lock" Cargo.lock
    else
        cp "$DDLOG_LOCK_DIR/program.Cargo.lock" Cargo.lock
    fi
fi
export CARGO_PROFILE_DEV_DEBUG=0
export CARGO_INCREMENTAL=0
if [ "${DDLOG_OFFLINE:-0}" = 1 ]; then
    "${DDLOG_CARGO:-cargo}" build --offline --locked --bin program_cli
else
    "${DDLOG_CARGO:-cargo}" build --bin program_cli
fi
cp "${CARGO_TARGET_DIR:-target}/debug/program_cli" "$output_path"
