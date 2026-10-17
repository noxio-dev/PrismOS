/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - Qt Widgets graphical interface, inspired by
 * Calamares: progress side panel, successive pages, live
 * validation, disk layout bar, real-time installation
 * log (worker thread).
 */

#pragma once

namespace prism::cli {
struct Args;      /* defined in Cli.h - reused by guiMain */
}

namespace prism::gui {

/* Creates the QApplication and runs the wizard. */
int guiMain(int argc, char** argv, const prism::cli::Args& args);

} // namespace prism::gui
