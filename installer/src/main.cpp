/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - single entry point.
 *
 *   prism-installer            -> Qt graphical interface (Calamares-like)
 *   prism-installer --cli      -> command-line wizard
 *   no display available       -> automatic fallback to CLI mode
 *
 * As soon as a real disk is targeted, administrator rights are
 * REQUESTED automatically (desktop dialog via pkexec in graphical
 * mode, sudo prompt in text mode) - the user no longer has to
 * relaunch anything by hand.  Rights-free usages (help, version,
 * image files, self-test) remain unchanged.
 */

#include "Cli.h"
#include "Elevate.h"

#include <cstdio>
#include <cstdlib>
#include <cstring>

/* Defined in Gui.cpp (creates the QApplication then the window). */
namespace prism::gui {
int guiMain(int argc, char** argv, const prism::cli::Args& args);
}

int main(int argc, char** argv)
{
    prism::cli::Args args = prism::cli::parseArgs(argc, argv);

    /* Administrator rights: obtain them BEFORE anything else, by
     * relaunching ourselves via pkexec/sudo (see Elevate.h).
     * shouldElevate() returns false for help, version, option errors,
     * image files, the self-test and PRISM_INSTALLER_NO_ELEVATE. */
    if (prism::elevate::shouldElevate(args)) {
        return prism::elevate::relaunchElevated(argc, argv, args);
    }

    /* Interface-free paths: help, version, option errors, explicit
     * CLI mode, non-interactive mode or --check audit. */
    if (args.cli || args.yes || args.help || args.version || args.check
        || !args.errors.empty()) {
        return prism::cli::cliMain(args);
    }

    /* Without a display server, switch cleanly to text mode.
     * An explicit QT_QPA_PLATFORM (e.g. offscreen for CI/self-test)
     * counts as a display: the user chose their platform, the text
     * fallback must not short-circuit it. */
    const char* x11 = std::getenv("DISPLAY");
    const char* way = std::getenv("WAYLAND_DISPLAY");
    const char* qpa = std::getenv("QT_QPA_PLATFORM");
    if ((!x11 || !*x11) && (!way || !*way) && (!qpa || !*qpa)) {
        std::fprintf(stderr,
                     "No display available: switching to text mode "
                     "(use --cli to request it explicitly).\n");
        return prism::cli::cliMain(args);
    }

    /* We are probably the re-elevated process (or a real root):
     * replace the original user's XDG_RUNTIME_DIR to avoid the flood
     * of Qt warnings "runtime directory not owned by UID 0".
     * No effect outside root, no effect under Wayland. */
    prism::elevate::sanitizeEnvAsRoot();

    return prism::gui::guiMain(argc, argv, args);
}
