/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - disk geometry and constants shared with
 * include/prism.inc (PrismOS BIOS boot chain).
 */

#pragma once

#include <cstdint>
#include <string>

namespace prism {

/* ---- Geometry --------------------------------------------------------- */

constexpr uint64_t SECTOR         = 512;             /* logical sector size       */
constexpr uint64_t MIB            = 1024 * 1024;
constexpr uint64_t MIB_SECTORS    = MIB / SECTOR;    /* 2048 */

/* Start LBA of the ESP (P1, first partition of the disk).  Some OEM
 * firmwares only try the default boot path (EFI/BOOT/BOOTX64.EFI) on
 * the FIRST partition of a removable medium: the ESP must therefore
 * come first.  The BIOS boot partition position does not matter -
 * boot0/boot1 are patched with its actual LBA at install time, and
 * the loader finds the ESP by scanning the GPT for its type GUID. */
constexpr uint64_t ESP_START         = 2048;
/* BIOS boot partition size: 2 MiB. */
constexpr uint64_t BOOT_PART_SECTORS = 4096;

/* ---- Boot chain constants (mirror of include/prism.inc) ---------------- */

constexpr uint32_t BOOT1_MAGIC       = 0x31425250;    /* "PRB1" */
constexpr uint64_t BOOT1_SECTORS     = 16;            /* 8 KiB reserved for boot1  */
constexpr uint64_t BOOT0_LBA_OFF     = 0x1B0;         /* qword: LBA of boot1, patched */
constexpr uint64_t LOADER_MAX_SIZE   = 0x00400000;    /* 4 MiB (loader limit)       */
constexpr uint32_t MBR_DISK_SIG      = 0x5052534D;    /* upstream disk signature     */

/* ---- Option bounds ----------------------------------------------------- */

constexpr uint64_t ESP_MIN_MIB       = 48;
constexpr uint64_t ESP_MAX_MIB       = 2048;
constexpr uint64_t ESP_DEFAULT_MIB   = 64;

/* Unallocated margin right after the ESP: this is where a larger
 * future kernel will be able to expand (the ESP will be grown into
 * this space during an update, without moving the other partitions). */
constexpr uint64_t RESERVE_MIN_MIB   = 0;
constexpr uint64_t RESERVE_MAX_MIB   = 4096;
constexpr uint64_t RESERVE_DEFAULT_MIB = 128;

constexpr uint64_t EXT4_MIN_MIB      = 32;
constexpr uint64_t EXT4_MAX_MIB      = 8 * 1024 * 1024;   /* 8 TiB and up */

/* Default labels. */
inline constexpr const char* ESP_LABEL   = "PRISMOS";
inline constexpr const char* EXT4_LABEL  = "PRISMDATA";

/* GPT partition names. */
inline constexpr const char* PART_NAME_BOOT = "PrismOS boot";
inline constexpr const char* PART_NAME_ESP  = "PrismOS ESP";
inline constexpr const char* PART_NAME_DATA = "PrismOS data";

/* GPT partition type GUIDs. */
inline constexpr const char* GUID_BIOS_BOOT =
    "21686148-6449-6E6F-744E-656564454649";
inline constexpr const char* GUID_ESP =
    "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
inline constexpr const char* GUID_LINUX_FS =
    "0FC63DAF-8483-4772-8E79-3D69D8477DE4";

/* Number of GPT entries written (128, like mkimage.py). */
constexpr uint32_t GPT_ENTRIES = 128;

/* Converts a size in bytes to a readable string ("1.5 GiB"). */
std::string humanSize(uint64_t bytes);

/* Converts MiB -> sectors. */
constexpr uint64_t mibToSectors(uint64_t mib) { return mib * MIB_SECTORS; }

} // namespace prism
