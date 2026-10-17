/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * Test binary: engine + CLI + elevation, without the Qt layer.
 * Makes it possible to validate all the installation logic (and the
 * automatic request for administrator privileges - tests T11 to T15)
 * with a plain C++ compiler.
 */

#include "../src/Cli.h"
#include "../src/Elevate.h"

int main(int argc, char** argv)
{
    prism::cli::Args args = prism::cli::parseArgs(argc, argv);

    /* Same logic as main.cpp, without the graphical layer: administrator
     * privileges are requested FIRST whenever a real disk is targeted
     * (the relaunch replaces this process, it does not come back). */
    if (prism::elevate::shouldElevate(args)) {
        return prism::elevate::relaunchElevated(argc, argv, args);
    }

    return prism::cli::cliMain(args);
}
