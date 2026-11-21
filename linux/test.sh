#!/bin/bash
# Bottle EQ on Linux, tried in a box that has no sound card: PipeWire and its session manager are
# started here, with one output device that plays to nowhere ("speakers"). Run from the repo's root:
#
#   docker run --rm -v "$PWD":/src -v bottle-eq-target:/target -v bottle-eq-cargo:/root/.cargo/registry bottle-eq-linux linux/test.sh
#
# What was captured and what the target was given are kept in test/linux/ (WAV), and a picture of the
# window in test/linux/window.bmp.
export XDG_RUNTIME_DIR=/tmp/run
mkdir -p $XDG_RUNTIME_DIR test/linux && chmod 700 $XDG_RUNTIME_DIR
cargo build --release 2>&1 | tail -1
cargo test --release 2>&1 | grep "test result"
exec dbus-run-session -- bash linux/inside.sh
