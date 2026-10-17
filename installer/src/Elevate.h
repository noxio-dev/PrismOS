/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - obtaining administrator privileges.
 *
 * Writing a disk (/dev/sdX, /dev/nvme...) requires being root.
 * Rather than failing with "Permission denied" and then asking the
 * user to relaunch by hand, the installer RE-EXECS ITSELF
 * with administrator privileges:
 *
 *   - graphical mode: pkexec (desktop authentication dialog,
 *     like Calamares), if needed "sudo -A" with a graphical
 *     input box (zenity/kdialog), as a last resort sudo;
 *   - text mode     : sudo (password prompt in the terminal).
 *
 * The relaunch preserves the original command line and passes the
 * display variables needed by the elevated process (DISPLAY,
 * XAUTHORITY, WAYLAND_DISPLAY...).  Usages requiring no particular
 * privileges (help, version, image files, self-test, PRISM_INSTALLER_NO_ELEVATE)
 * trigger NO elevation at all.
 *
 * No Qt dependency: usable before the creation of QApplication.
 */

#pragma once

#include "Cli.h"

namespace prism::elevate {

/* Return code when administrator privileges could not be
 * obtained (elevation tool missing, execution impossible). */
constexpr int ELEVATE_FAILED = 4;

/* True if the current execution must obtain administrator privileges
 * before continuing (see the file header for the rules). */
bool shouldElevate(const cli::Args& args);

/* Relaunches the current binary with administrator privileges,
 * passing argv through as-is.  Returns - with ELEVATE_FAILED -
 * only when no elevation method could be attempted: if the user
 * refuses, sudo/pkexec (which replaced us) returns its own
 * exit code. */
int relaunchElevated(int argc, char** argv, const cli::Args& args);

/* Sanitizes the environment of the GUI process once root (called by
 * main.cpp just before guiMain): after a pkexec/sudo elevation,
 * XDG_RUNTIME_DIR still points to the user's directory
 * (/run/user/1000) and Qt emits six "runtime directory
 * is not owned by UID 0" warnings before EACH access.  We replace it
 * with a private 0700 directory owned by root - except under Wayland,
 * whose socket lives precisely in the user's directory. */
void sanitizeEnvAsRoot();

} // namespace prism::elevate
