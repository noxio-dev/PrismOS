/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - command-line interface (--cli mode):
 * Debian/Calamares-style interactive wizard, ANSI colors, progress
 * bar, non-interactive mode (--yes) and dry run (--dry-run).
 *
 * No Qt dependency: usable without a display server.
 */

#pragma once

#include "InstallOptions.h"

#include <string>
#include <vector>

namespace prism::cli {

/* Command-line arguments (shared GUI/CLI). */
struct Args {
    InstallOptions options;

    bool cli = false;
    bool yes = false;             /* non-interactive confirmation        */
    bool help = false;
    bool version = false;
    bool check = false;           /* read-only audit (--check)           */
    bool guiSelfTest = false;     /* internal: drives the GUI mouse-free */

    std::vector<std::string> errors;   /* parsing problems               */
};

/* Parse argv (without the program name). */
Args parseArgs(int argc, char** argv);

/* Help text. */
std::string usageText();

/* Entry point of CLI mode; returns an exit code:
 *   0 success, 1 usage, 2 validation error, 3 installation failure
 *     (or problems detected by --check),
 *   4 administrator rights not obtained (elevation impossible - see
 *     Elevate.h, only produced by the full binary),
 *   130 interrupted. */
int cliMain(const Args& args);

} // namespace prism::cli
