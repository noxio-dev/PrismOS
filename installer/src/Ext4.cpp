/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - ext4 formatting implementation.
 */

#include "Ext4.h"

#include <sstream>
#include <stdexcept>
#include <unistd.h>

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

StatResult statFile(const std::string& target, uint64_t offsetBytes,
                    const std::string& path)
{
    StatResult res;

    std::string debugfs = which("debugfs");
    if (debugfs.empty()) {
        /* sbin directories are often missing from a user's PATH. */
        for (const char* dir : {"/usr/sbin", "/sbin", "/usr/local/sbin"}) {
            std::string cand = std::string(dir) + "/debugfs";
            if (::access(cand.c_str(), X_OK) == 0) { debugfs = cand; break; }
        }
    }
    if (debugfs.empty()) {
        res.detail = "debugfs not found (e2fsprogs)";
        return res;
    }

    /* libext2fs syntax for a region inside a file: "image?offset=N". */
    std::string spec = target;
    if (offsetBytes > 0)
        spec += "?offset=" + std::to_string(offsetBytes);

    ProcessResult r;
    try {
        r = runProcess(debugfs, {"-R", "stat " + path, spec}, {}, 60);
    } catch (const std::exception& ex) {
        res.detail = std::string("debugfs: ") + ex.what();
        return res;
    }

    /* debugfs exits 0 even when the command fails: read its output. */
    const std::string out = r.stdoutText + "\n" + r.stderrText;

    if (out.find("File not found") != std::string::npos) {
        res.kind = StatKind::NotFound;
        res.detail = path + " is not in the ext4 filesystem";
        return res;
    }

    /* "User: 0 Group: 0 Project: 0   Size: 3145728" - the first "Size:"
     * of the output is the inode's (the "Fragment: ... Size: 0" field
     * comes after it). */
    auto pos = out.find("Size:");
    if (r.ok() && out.find("Inode:") != std::string::npos
        && pos != std::string::npos) {
        size_t i = pos + 5;
        while (i < out.size() && out[i] == ' ') ++i;
        size_t j = i;
        while (j < out.size() && out[j] >= '0' && out[j] <= '9') ++j;
        if (j > i) {
            res.size = std::stoull(out.substr(i, j - i));
            res.kind = StatKind::Ok;
            return res;
        }
    }

    /* Last meaningful line (the first one is debugfs's version banner). */
    std::string reason;
    {
        std::istringstream is(out);
        std::string line;
        while (std::getline(is, line))
            if (!line.empty() && line.rfind("debugfs ", 0) != 0)
                reason = line;
    }
    if (reason.size() > 160) reason.resize(160);
    res.detail = "debugfs could not read the filesystem"
                 + (reason.empty() ? std::string() : " (" + reason + ")");
    return res;
}

} // namespace prism::ext4
