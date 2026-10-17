/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - read-only boot audit (--check mode): examines a
 * disk or an image WITHOUT WRITING ANYTHING and answers the question
 * "is this USB drive bootable, and if not, why?".
 *
 * Checked chain, in the order a firmware walks it:
 *   1. protective MBR (0x55AA signature, 0xEE entry);
 *   2. primary and backup GPT (signatures, CRCs, bounds);
 *   3. EFI System Partition (type GUID C12A7328-...): full FAT32
 *      volume (BPB, FAT, chains, LFN, backup sectors,
 *      FSInfo) via verify::auditEsp;
 *   4. UEFI removable boot path: EFI/BOOT/BOOTX64.EFI and its
 *      PE/COFF header (x86-64 EFI application);
 *   5. BIOS boot partition (boot1, "PRB1" magic).
 */

#pragma once

#include <cstdint>
#include <string>
#include <vector>

namespace prism::check {

struct Report {
    /* Diagnostic lines (everything observed, in order). */
    std::vector<std::string> lines;

    /* Blocking defects (empty = disk bootable in BIOS + UEFI). */
    std::vector<std::string> problems;

    bool ok() const { return problems.empty(); }
};

/* Audits `path' (block device /dev/... or image file) read-only.
 * Never throws: any incident becomes an entry in Report::problems. */
Report auditTarget(const std::string& path);

} // namespace prism::check
