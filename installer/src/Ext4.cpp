/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - ext4 formatting implementation.
 */

#include "Ext4.h"

#include <stdexcept>

namespace prism::ext4 {

std::string findMke2fs(const std::string& overridePath)
{
    if (!overridePath.empty())
        return overridePath;
    return which("mke2fs");
}

void format(const FormatRequest& req)
{
    std::string mke2fs = findMke2fs(req.mke2fsPath);
    if (mke2fs.empty())
        throw std::runtime_error(
            "mke2fs not found: install e2fsprogs "
            "(the \"e2fsprogs\" package on most distributions)");

    if (req.label.size() > 16)
        throw std::runtime_error(
            "the ext4 label must be at most 16 characters long");

    std::vector<std::string> args;
    args.push_back("-q");
    args.push_back("-F");                       /* overwrite files and disks in use */
    args.push_back("-t");
    args.push_back("ext4");
    args.push_back("-b");
    args.push_back("4096");
    if (!req.label.empty()) {
        args.push_back("-L");
        args.push_back(req.label);
    }
    if (!req.seedDir.empty()) {
        args.push_back("-d");
        args.push_back(req.seedDir);
    }

    if (req.offsetBytes > 0) {
        /* Image mode: region inside a file.  The size must
         * imperatively be explicit (in 4096-byte blocks) so as not
         * to spill over into the backup GPT. */
        args.push_back("-E");
        args.push_back("offset=" + std::to_string(req.offsetBytes));
        args.push_back(req.target);
        uint64_t blocks = req.sizeBytes / 4096;
        args.push_back(std::to_string(blocks));
    } else {
        /* Device mode: the partition node bounds the size. */
        args.push_back(req.target);
    }

    ProcessResult r = runProcess(mke2fs, args, {}, 600 /* 10 min */);
    if (!r.ok()) {
        std::string detail = r.stderrText;
        if (detail.size() > 2000) detail.resize(2000);
        throw std::runtime_error(
            "mke2fs failed (code " + std::to_string(r.exitCode) + "): "
            + detail);
    }
}

} // namespace prism::ext4
