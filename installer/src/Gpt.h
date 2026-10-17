/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - builds the GPT partition table, the protective
 * MBR (boot0) and the contents of the BIOS boot partition
 * (boot1 + 64-bit loader), with the LBA patches expected
 * by the PrismOS boot chain.
 *
 * The layout produced is identical to tools/mkimage.py:
 *
 *   LBA 0            protective MBR: boot0 code, signature, 0xEE entry
 *   LBA 1            GPT header
 *   LBA 2..33        128 partition entries
 *   LBA 2048..4095   "PrismOS boot"  : boot1 + loader (raw sectors)
 *   LBA 4096..       "PrismOS ESP"   : FAT32 (EFI, scripts, kernel)
 *                    (unallocated reserved space, for kernel growth)
 *                    "PrismOS data"  : ext4
 *   last 33 LBAs     backup GPT (entries + header)
 */

#pragma once

#include "Layout.h"
#include "Util.h"

#include <cstdint>
#include <string>
#include <vector>

namespace prism {

/* Partitioning plan computed from the options. */
struct PartitionPlan {
    /* Absolute LBAs (512-byte sectors). */
    uint64_t totalSectors  = 0;      /* final disk size                    */
    uint64_t lastLba       = 0;      /* totalSectors - 1                   */
    uint64_t firstUsable   = 34;
    uint64_t lastUsable    = 0;

    /* Order on the disk: ESP (P1) first, then the unallocated
     * reserve, then the BIOS boot partition (P2), then ext4 (P3).
     * The ESP is first because some OEM firmwares only try the
     * default boot path on the FIRST partition of removable media. */
    uint64_t espStart      = ESP_START;
    uint64_t espSectors    = 0;

    /* Unallocated growth margin, right after the ESP. */
    uint64_t reserveSectors = 0;

    /* BIOS boot partition (P2): LBA computed by the plan (after the
     * reserve); boot0/boot1 are patched with it at install time. */
    uint64_t bootStart     = 0;
    uint64_t bootSectors   = BOOT_PART_SECTORS;

    uint64_t ext4Start     = 0;
    uint64_t ext4Sectors   = 0;

    /* Identifiers (logical representation). */
    util::Uuid diskGuid;
    util::Uuid bootGuid, espGuid, ext4Guid;

    std::string espLabel;             /* FAT32 label (11 bytes max)  */
    std::string ext4Label;            /* ext4 label (16 bytes max)   */

    uint64_t diskBytes()  const { return totalSectors * SECTOR; }
    uint64_t espBytes()   const { return espSectors * SECTOR; }
    uint64_t ext4Bytes()  const { return ext4Sectors * SECTOR; }
    uint64_t reserveBytes() const { return reserveSectors * SECTOR; }
};

/* Raw components to write to the disk. */
struct BootArtifacts {
    std::vector<uint8_t> boot0;       /* exactly 512 bytes            */
    std::vector<uint8_t> boot1;       /* BOOT1_SECTORS * 512 bytes    */
    std::vector<uint8_t> loader;      /* 64-bit BIOS loader image     */

    /* Size/magic validation (like mkimage.py).  Returns ""
     * if all is well, otherwise an explicit error message. */
    std::string validate() const;
};

/* Builds the protective MBR sector: boot0 code (patched with the boot
 * partition LBA), disk signature, 0xEE entry, 0x55AA. */
std::vector<uint8_t> buildProtectiveMbr(const BootArtifacts& art,
                                        const PartitionPlan& plan);

/* Builds the primary GPT header (LBA 1). */
std::vector<uint8_t> buildGptHeader(const PartitionPlan& plan,
                                    uint32_t entriesCrc,
                                    bool backup);

/* Builds the GPT entries table (128 entries of 128 bytes). */
std::vector<uint8_t> buildGptEntries(const PartitionPlan& plan);

/* Patches boot1: loader LBA (u64 @ 4) and sector count (u32 @ 12),
 * then returns the raw contents of the boot partition:
 * boot1 (8 KiB) followed by the loader. */
std::vector<uint8_t> buildBootPartition(const BootArtifacts& art,
                                        const PartitionPlan& plan);

} // namespace prism
