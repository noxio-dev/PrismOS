#!/bin/sh
#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# Configure, build, then run the prism-installer CLI test suite.
#
#   ./build.sh [--no-tests]
#

set -e
HERE=$(cd "$(dirname "$0")" && pwd)

# Qt outside the standard paths?  Uncomment/adapt:
# QT_PREFIX=${QT_PREFIX:-$HOME/Qt/6.8.3/gcc_64}

# The arguments are accumulated in "$@" (always correctly quoted by
# the shell, even with spaces) rather than in an unquoted interpolated
# variable.
if [ -n "${QT_PREFIX:-}" ]; then
    set -- "-DCMAKE_PREFIX_PATH=$QT_PREFIX" "$@"
fi

echo "== prism-installer: configuration =="
cmake -B "$HERE/build" -S "$HERE" -DCMAKE_BUILD_TYPE=Release "$@"

echo
echo "== build =="
cmake --build "$HERE/build" -j"$(nproc 2>/dev/null || echo 2)"

echo
echo "== binary: $HERE/build/prism-installer =="

if [ "${1:-}" = "--no-tests" ]; then
    exit 0
fi

echo
echo "== engine tests (no Qt, mke2fs stub, disk images) =="
if [ ! -x "$HERE/build/prism-installer-cli" ]; then
    g++ -std=c++20 -O2 -Wall -Wextra \
        -o "$HERE/build/prism-installer-cli" \
        "$HERE/tests/cli_main.cpp" \
        "$HERE/src/Util.cpp" "$HERE/src/Gpt.cpp" "$HERE/src/Fat32.cpp" \
        "$HERE/src/DiskScan.cpp" "$HERE/src/Process.cpp" \
        "$HERE/src/Ext4.cpp" "$HERE/src/Installer.cpp" \
        "$HERE/src/Verifier.cpp" "$HERE/src/Cli.cpp" \
        "$HERE/src/Elevate.cpp" "$HERE/src/Pe.cpp" \
        "$HERE/src/BootCheck.cpp"
fi
sh "$HERE/tests/run_cli_tests.sh" "$HERE/build/prism-installer-cli"
