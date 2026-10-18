/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - installation options (shared CLI / GUI).
 */

#pragma once

#include "Layout.h"

#include <cstdint>
#include <string>

namespace prism {

struct InstallOptions {
    /* PrismOS build directory (contains boot0.bin,
     * boot1.bin, loader.bin, esp/ and root/). */
    std::string sourceDir;

    /* Optional ESP directory override (otherwise <sourceDir>/esp):
     * tree copied into the FAT32 ESP (P1), i.e. EFI/BOOT/BOOTX64.EFI. */
    std::string espDir;

    /* Optional root directory override (otherwise <sourceDir>/root):
     * tree copied into the ext4 root (P3), i.e. boot/kernel/kernel,
     * boot/lua, boot/loader.conf, bin, etc, usr. */
    std::string rootDir;

    /* Target: block device (/dev/sdX) or image file. */
    std::string target;

    /* Image mode: if the file does not exist, create it with this
     * size (K/M/G/T suffixes, powers of 1024). 0 = disabled. */
    uint64_t imageSize = 0;

    /* ESP size (FAT32: only the UEFI stub). */
    uint64_t espMiB = ESP_DEFAULT_MIB;

    /* ext4 root partition size; 0 = all remaining space. */
    uint64_t rootMiB = 0;

    std::string espLabel  = ESP_LABEL;
    std::string rootLabel = ROOT_LABEL;

    /* mke2fs path override (automated tests). */
    std::string mke2fsPath;

    /* Create a "PrismOS" entry in the firmware boot menu
     * (efibootmgr) after installation: useful when the firmware
     * treats the USB drive as a FIXED disk and therefore does not try
     * the default \EFI\BOOT\BOOTX64.EFI path for removable media. */
    bool efiBootEntry = false;

    /* efibootmgr path override (automated tests). */
    std::string efibootmgrPath;

    /* Simulation: validate and display everything, write nothing. */
    bool dryRun = false;
};

/* Parses a textual size: "64M", "8G", "512K", "1T", "1024".
 * Returns 0 on error. */
uint64_t parseSize(const std::string& s);

} // namespace prism
