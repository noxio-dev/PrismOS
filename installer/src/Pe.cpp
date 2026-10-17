/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - PE/COFF parser implementation (see Pe.h).
 */

#include "Pe.h"

#include <cstring>

namespace prism::pe {

namespace {

inline uint16_t le16(const uint8_t* p)
{
    return uint16_t(p[0] | (p[1] << 8));
}

inline uint32_t le32(const uint8_t* p)
{
    return uint32_t(p[0]) | (uint32_t(p[1]) << 8)
           | (uint32_t(p[2]) << 16) | (uint32_t(p[3]) << 24);
}

} // namespace

Info parse(const uint8_t* data, size_t len)
{
    Info info;
    info.fileSize = len;

    if (len < 0x40) {
        info.error = "file too short (" + std::to_string(len)
                     + " bytes) for a PE executable";
        return info;
    }
    if (data[0] != 'M' || data[1] != 'Z') {
        info.error = "DOS \"MZ\" header missing - the file is not a"
                     " PE/COFF executable";
        return info;
    }

    uint32_t peOff = le32(data + 0x3C);
    if (peOff < 0x40 || peOff + 4 + 20 > len) {
        info.error = "PE header offset out of bounds (e_lfanew = "
                     + std::to_string(peOff) + ")";
        return info;
    }
    const uint8_t* pe = data + peOff;
    if (std::memcmp(pe, "PE\0\0", 4) != 0) {
        info.error = "\"PE\\0\\0\" signature missing at e_lfanew";
        return info;
    }

    const uint8_t* coff = pe + 4;
    info.machine = le16(coff + 0);
    info.numberOfSections = le16(coff + 2);
    info.sizeOfOptionalHeader = le16(coff + 16);
    info.characteristics = le16(coff + 18);

    if (info.sizeOfOptionalHeader < 70) {
        info.error = "optional header too short ("
                     + std::to_string(info.sizeOfOptionalHeader)
                     + " bytes) - subsystem unreadable";
        return info;
    }
    if (size_t(peOff) + 4 + 20 + info.sizeOfOptionalHeader > len) {
        info.error = "optional header truncated (incomplete file?)";
        return info;
    }

    const uint8_t* opt = coff + 20;
    info.optionalMagic = le16(opt + 0);
    info.addressOfEntryPoint = le32(opt + 16);
    info.sizeOfImage = le32(opt + 56);
    info.subsystem = le16(opt + 68);

    /* Minimal consistency of the section table: it follows the
     * optional header; a file that stops before that is truncated. */
    size_t sectionsOff = size_t(peOff) + 4 + 20 + info.sizeOfOptionalHeader;
    if (info.numberOfSections > 96) {
        info.error = "implausible number of sections ("
                     + std::to_string(info.numberOfSections) + ")";
        return info;
    }
    if (size_t(info.numberOfSections) * 40 + sectionsOff > len) {
        info.error = "section table truncated (incomplete file?)";
        return info;
    }

    info.valid = true;
    return info;
}

const char* machineName(uint16_t machine)
{
    switch (machine) {
    case MACHINE_I386:  return "x86 (32-bit)";
    case MACHINE_AMD64: return "x86-64";
    case MACHINE_ARM32: return "ARM (32-bit)";
    case MACHINE_ARM64: return "AArch64";
    default:            return "unknown";
    }
}

const char* subsystemName(uint16_t subsystem)
{
    switch (subsystem) {
    case SUBSYSTEM_WINDOWS_GUI:         return "Windows GUI application";
    case SUBSYSTEM_WINDOWS_CUI:         return "Windows console application";
    case SUBSYSTEM_EFI_APPLICATION:     return "EFI application";
    case SUBSYSTEM_EFI_BOOT_DRIVER:     return "EFI boot driver";
    case SUBSYSTEM_EFI_RUNTIME_DRIVER:  return "EFI runtime driver";
    default:                            return "unknown";
    }
}

std::string validateEfiApp(const Info& info)
{
    if (!info.valid)
        return info.error.empty() ? "invalid PE structure" : info.error;

    if (info.machine != MACHINE_AMD64)
        return std::string("machine ")
               + machineName(info.machine)
               + " - BOOTX64.EFI must be an x86-64 application"
                 " (64-bit firmware)";

    if (info.optionalMagic != 0x20B)
        return "PE32 (32-bit) header instead of PE32+ - an x86-64"
               " firmware only loads PE32+ images";

    if (info.subsystem != SUBSYSTEM_EFI_APPLICATION)
        return std::string("subsystem \"")
               + subsystemName(info.subsystem)
               + "\" instead of \"EFI application\" (10)";

    if (info.addressOfEntryPoint == 0)
        return "null entry point - the firmware would have nothing to run";

    if (info.sizeOfImage == 0)
        return "SizeOfImage is zero";

    return "";
}

std::string validateEfiAppBytes(const uint8_t* data, size_t len)
{
    return validateEfiApp(parse(data, len));
}

} // namespace prism::pe
