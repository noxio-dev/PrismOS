/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - formats the ext4 data partition via
 * mke2fs (e2fsprogs), the way the project's `make data` does.
 *
 * Two modes:
 *   - device: mke2fs directly formats the partition node
 *     (/dev/sdX3), whose size is already the region's size;
 *   - image file: mke2fs works inside the image with
 *     -E offset=N and an explicit size in 4 KiB blocks, so as to
 *     never touch the backup GPT located at the end of the disk.
 */

#pragma once

#include "Process.h"

#include <cstdint>
#include <string>
#include <vector>

namespace prism::ext4 {

struct FormatRequest {
    /* Block device OR image file (in that case offsetBytes > 0). */
    std::string target;
    uint64_t offsetBytes = 0;        /* 0 for a partition node          */
    uint64_t sizeBytes = 0;          /* mandatory in image mode         */
    std::string label;               /* ext4 label (16 bytes max)       */
    std::string seedDir;             /* optional: tree to populate      */
    std::string mke2fsPath;          /* override (tests)                */
};

/* Runs the formatting.  Throws std::runtime_error if mke2fs is missing
 * or fails (the output is included in the message). */
void format(const FormatRequest& req);

/* Returns the path of mke2fs (PATH or override), "" if absent. */
std::string findMke2fs(const std::string& overridePath);

/* Result of statFile(). */
enum class StatKind {
    Ok,           /* the file exists, `size` is valid                       */
    NotFound,     /* the filesystem was read and the file is NOT in it      */
    Unavailable,  /* could not tell (no debugfs, unreadable filesystem...)  */
};

struct StatResult {
    StatKind kind = StatKind::Unavailable;
    uint64_t size = 0;
    std::string detail;              /* human-readable reason when != Ok */
};

/* Looks a file up INSIDE the ext4 filesystem with debugfs (e2fsprogs),
 * read-only: `target` is a block device (offsetBytes = 0) or an image
 * file (offsetBytes = start of the partition).  Used after the install
 * to prove that the kernel really landed in the root filesystem. */
StatResult statFile(const std::string& target, uint64_t offsetBytes,
                    const std::string& path);

} // namespace prism::ext4
