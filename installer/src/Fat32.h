/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - builds a FAT32 volume (ESP) entirely in C++, with
 * no external dependency (neither mkfs.fat nor mtools).  Writes:
 *
 *   - the FAT32 boot sector (BPB) + its backup copy,
 *   - the FSInfo sectors (primary and backup),
 *   - two copies of the FAT (kept in memory then written),
 *   - the complete source directory tree, with long names
 *     (LFN) when a name does not fit in 8.3.
 *
 * Data is written as we go (pwrite): the memory required stays
 * limited to the FATs and the cluster buffers.
 *
 * The geometry (sectors/cluster) follows the Microsoft / dosfstools
 * table: the smallest grouping yielding a valid FAT32 cluster count
 * (>= 65525) is chosen, based on the volume size.
 */

#pragma once

#include <cstdint>
#include <functional>
#include <string>
#include <vector>

namespace prism::fat32 {

struct BuildResult {
    uint64_t volumeBytes = 0;
    uint32_t sectorsPerCluster = 0;
    uint32_t clusterCount = 0;      /* data clusters                   */
    uint32_t usedClusters = 0;
    size_t   files = 0;             /* files copied                    */
    size_t   dirs = 0;              /* subdirectories created          */
    uint64_t payloadBytes = 0;      /* total data copied               */
};

/* Builds a FAT32 volume of `volumeBytes` bytes containing the tree
 * under `srcDir`, written directly at offset `baseOffset` of
 * descriptor `fd` (image file or block device, already open).
 * `onFile` is called for each file copied (relative path).
 * Throws std::runtime_error on failure. */
BuildResult buildVolume(int fd, uint64_t baseOffset, uint64_t volumeBytes,
                        const std::string& volumeLabel,
                        const std::string& srcDir,
                        const std::function<void(const std::string&)>& onFile);

} // namespace prism::fat32
