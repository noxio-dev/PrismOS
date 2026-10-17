/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - a posteriori verification of the installation:
 * re-reads the disk and independently checks what was written.
 * This module shares NO code with the FAT32 writer: it re-interprets
 * the on-disk structures for a true cross-check (boot sector,
 * FSInfo, FAT, directories, file CRCs).
 *
 * Two levels:
 *   - verifyEsp()        : compares the volume against the source tree
 *                          (after installation);
 *   - auditEsp()         : audits the volume WITHOUT a source - FAT32
 *                          structure, chains, LFN (checksums), backups,
 *                          then the UEFI boot path EFI/BOOT/BOOTX64.EFI
 *                          and PE/COFF validity (--check mode, diagnosis
 *                          of an already-installed USB drive).
 */

#pragma once

#include <cstdint>
#include <functional>
#include <string>
#include <vector>

namespace prism::verify {

/* Result of the standalone audit of a FAT32 volume (ESP). */
struct EspAudit {
    bool readable = false;         /* analyzable FAT32 volume             */
    std::vector<std::string> problems;

    /* Informative fields (valid if readable). */
    std::string volumeLabel;
    uint32_t sectorsPerCluster = 0;
    uint32_t clusterCount = 0;
    uint32_t usedClusters = 0;
    uint32_t freeClusters = 0;
    size_t files = 0;
    size_t dirs = 0;
    uint64_t payloadBytes = 0;     /* sum of file sizes                   */

    bool hasBootx64 = false;
    uint64_t bootx64Bytes = 0;
    std::string bootx64Detail;     /* description or PE error             */

    bool ok() const { return problems.empty(); }
};

/* Verifies the ESP (FAT32 volume at baseOffset) against the srcDir tree:
 * walks the directories, follows the cluster chains, re-reads each file
 * and compares the CRC32s.  Returns the list of problems found
 * (empty = compliant). */
std::vector<std::string> verifyEsp(int fd, uint64_t baseOffset,
                                   const std::string& srcDir,
                                   const std::function<void(const std::string&)>& onFile);

/* Audits the FAT32 volume at baseOffset WITHOUT a source tree:
 * structures, cluster chains, LFN checksums, backup sectors,
 * FSInfo, full file walk, and the UEFI boot path
 * (EFI/BOOT/BOOTX64.EFI: presence + PE/COFF validity). */
EspAudit auditEsp(int fd, uint64_t baseOffset);

/* Targeted variant for post-installation verification: only checks the
 * UEFI boot path (EFI/BOOT/BOOTX64.EFI read from the disk + PE header).
 * Returns the list of problems (empty = bootable). */
std::vector<std::string> verifyUefiBoot(int fd, uint64_t baseOffset);

/* Verifies the low-level structures: protective MBR, GPT headers
 * (primary and backup), entry CRCs, range consistency,
 * boot partition (magic + patches + bootloader bytes). */
std::vector<std::string> verifyGptBoot(int fd,
                                       const std::vector<uint8_t>& expectedMbr,
                                       const std::vector<uint8_t>& expectedEntries,
                                       uint64_t totalSectors,
                                       uint64_t bootStart,
                                       const std::vector<uint8_t>& expectedBootPart);

} // namespace prism::verify
