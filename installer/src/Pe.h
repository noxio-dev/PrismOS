/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - (minimal) PE/COFF header parsing: used to
 * validate that the UEFI bootstrap (EFI/BOOT/BOOTX64.EFI) is indeed
 * an x86-64 EFI application a firmware can execute.
 *
 * A UEFI firmware loads BOOTX64.EFI via LoadImage(), which applies the
 * PE/COFF specification: "MZ" file, "PE\0\0" signature,
 * Machine = AMD64, subsystem = 10 (EFI application).  A truncated
 * bootstrap, one of another architecture (e.g. ARM64) or of another
 * type (DLL, driver) is rejected SILENTLY by the firmware - the key
 * then appears "not bootable" with no explanation.  This module makes
 * it possible to catch this case before installation and to diagnose
 * an already-installed key (--check mode).
 */

#pragma once

#include <cstddef>
#include <cstdint>
#include <string>

namespace prism::pe {

/* Fields extracted from the header (0 = unknown / absent). */
struct Info {
    bool valid = false;             /* PE structure readable and complete */
    uint16_t machine = 0;           /* IMAGE_FILE_MACHINE_*               */
    uint16_t subsystem = 0;         /* IMAGE_SUBSYSTEM_*                  */
    uint16_t characteristics = 0;
    uint16_t numberOfSections = 0;
    uint16_t sizeOfOptionalHeader = 0;
    uint16_t optionalMagic = 0;     /* 0x10B = PE32, 0x20B = PE32+        */
    uint32_t addressOfEntryPoint = 0;
    uint32_t sizeOfImage = 0;
    uint64_t fileSize = 0;

    std::string error;              /* first reason of invalidity         */
};

/* Machines (relevant subset). */
constexpr uint16_t MACHINE_I386  = 0x014C;
constexpr uint16_t MACHINE_AMD64 = 0x8664;
constexpr uint16_t MACHINE_ARM32 = 0x01C4;
constexpr uint16_t MACHINE_ARM64 = 0xAA64;

/* Subsystems (subset). */
constexpr uint16_t SUBSYSTEM_WINDOWS_GUI    = 2;
constexpr uint16_t SUBSYSTEM_WINDOWS_CUI    = 3;
constexpr uint16_t SUBSYSTEM_EFI_APPLICATION = 10;
constexpr uint16_t SUBSYSTEM_EFI_BOOT_DRIVER = 11;
constexpr uint16_t SUBSYSTEM_EFI_RUNTIME_DRIVER = 12;

/* Parses the first len bytes of data.  Never throws an exception:
 * any problem is described in Info::error (and valid = false). */
Info parse(const uint8_t* data, size_t len);

/* Human-readable names for messages. */
const char* machineName(uint16_t machine);
const char* subsystemName(uint16_t subsystem);

/* Validation of an x86-64 UEFI bootstrap (BOOTX64.EFI): returns "" if
 * the file is a well-formed AMD64 EFI application, otherwise the first
 * reason for rejection, in English. */
std::string validateEfiApp(const Info& info);
std::string validateEfiAppBytes(const uint8_t* data, size_t len);

} // namespace prism::pe
