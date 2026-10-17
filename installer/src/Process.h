/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - external program execution (mke2fs...) via
 * fork/exec, with output capture.  No Qt dependency.
 */

#pragma once

#include <cstdint>
#include <map>
#include <string>
#include <vector>

namespace prism {

struct ProcessResult {
    int exitCode = -1;
    std::string stdoutText;
    std::string stderrText;
    std::string commandLine;      /* for logs                    */

    bool ok() const { return exitCode == 0; }
};

/* Runs `program` with the arguments `args`.  `env` adds environment
 * variables (PRISM_MKE2FS=...).  Throws std::runtime_error
 * if the program is not found.  Timeout in seconds (0 = unlimited). */
ProcessResult runProcess(const std::string& program,
                         const std::vector<std::string>& args,
                         const std::map<std::string, std::string>& env = {},
                         unsigned timeoutSeconds = 0);

/* Runs a program, looking it up in $PATH. */
ProcessResult runProcessPath(const std::string& name,
                             const std::vector<std::string>& args,
                             const std::map<std::string, std::string>& env = {},
                             unsigned timeoutSeconds = 0);

/* Returns the absolute path of a program from $PATH, "" otherwise. */
std::string which(const std::string& name);

} // namespace prism
