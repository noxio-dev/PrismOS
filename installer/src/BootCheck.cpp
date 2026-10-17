/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - --check audit implementation (see BootCheck.h).
 */

#include "BootCheck.h"
#include "Layout.h"
#include "Util.h"
#include "Verifier.h"

#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <filesystem>
#include <set>

#include <linux/fs.h>
#include <sys/ioctl.h>
#include <unistd.h>

namespace prism::check {

namespace {

void preadAll(int fd, uint64_t off, void* buf, size_t len)
{
    uint8_t* p = static_cast<uint8_t*>(buf);
    while (len > 0) {
        ssize_t n = ::pread(fd, p, len, off_t(off));
        if (n < 0) throw std::runtime_error(std::string("pread failed: ")
                                            + std::strerror(errno));
        if (n == 0) throw std::runtime_error("read past the end");
        off += uint64_t(n);
        p += n;
        len -= size_t(n);
    }
}

uint16_t le16(const uint8_t* p) { return uint16_t(p[0] | (p[1] << 8)); }
uint32_t le32(const uint8_t* p)
{
    return uint32_t(p[0]) | (uint32_t(p[1]) << 8)
           | (uint32_t(p[2]) << 16) | (uint32_t(p[3]) << 24);
}
uint64_t le64(const uint8_t* p)
{
    return uint64_t(le32(p)) | (uint64_t(le32(p + 4)) << 32);
}

/* RFC 4122 representation from the GPT little-endian form
 * (identical to verify_image.py). */
std::string guidToString(const uint8_t* raw)
{
    char buf[40];
    uint32_t d1 = le32(raw);
    uint16_t d2 = le16(raw + 4), d3 = le16(raw + 6);
    std::snprintf(buf, sizeof(buf),
                  "%08X-%04X-%04X-%02X%02X-%02X%02X%02X%02X%02X%02X",
                  d1, d2, d3, raw[8], raw[9], raw[10], raw[11],
                  raw[12], raw[13], raw[14], raw[15]);
    return buf;
}

struct PartitionEntry {
    int number = 0;                /* 1..128 (GPT number)                */
    std::string typeGuid;
    std::string name;              /* decoded UTF-16LE (approximate)     */
    uint64_t first = 0, last = 0;
    uint64_t attrs = 0;            /* GPT flags (bit 2 = legacy
                                   * BIOS bootable)                    */
};

std::string decodeName(const uint8_t* p, size_t len)
{
    std::string out;
    for (size_t i = 0; i + 1 < len; i += 2) {
        uint16_t ch = le16(p + i);
        if (!ch) break;
        out += ch < 0x80 ? char(ch) : '?';
    }
    return out;
}

struct FdGuard {
    int fd;
    explicit FdGuard(int f) : fd(f) {}
    ~FdGuard() { if (fd >= 0) ::close(fd); }
};

} // namespace

Report auditTarget(const std::string& path)
{
    Report rep;

    int fd = ::open(path.c_str(), O_RDONLY);
    if (fd < 0) {
        rep.problems.push_back("unable to open: " + path + ": "
                               + std::strerror(errno)
                               + " (administrator rights required to"
                                 " read a disk?)");
        return rep;
    }
    FdGuard guard(fd);

    /* ---- Target size ------------------------------------------------- */
    uint64_t bytes = 0;
    if (util::isBlockDevice(path)) {
        if (ioctl(fd, BLKGETSIZE64, &bytes) != 0) {
            rep.problems.push_back(std::string("disk size unreadable: ")
                                   + std::strerror(errno));
            return rep;
        }
    } else {
        bytes = uint64_t(::lseek(fd, 0, SEEK_END));
        if (bytes == uint64_t(-1)) {
            rep.problems.push_back("file size unreadable");
            return rep;
        }
    }
    uint64_t totalSectors = bytes / SECTOR;
    rep.lines.push_back("target: " + path + " - " + humanSize(bytes) + " ("
                        + std::to_string(totalSectors) + " sectors)");
    if (totalSectors < 40) {
        rep.problems.push_back("too small to hold a GPT disk");
        return rep;
    }

    try {
        /* ---- 1. Protective MBR --------------------------------------- */
        uint8_t mbr[512];
        preadAll(fd, 0, mbr, sizeof(mbr));
        if (mbr[0x1FE] != 0x55 || mbr[0x1FF] != 0xAA)
            rep.problems.push_back("MBR: 0x55AA signature missing (LBA 0)"
                                   " - most firmwares refuse"
                                   " the disk");
        bool protective = false;
        for (int i = 0; i < 4; ++i) {
            if (mbr[0x1BE + i * 16 + 4] == 0xEE) protective = true;
        }
        if (!protective)
            rep.problems.push_back("MBR: no protective 0xEE entry -"
                                   " a legacy tool could treat the disk"
                                   " as unpartitioned");
        else
            rep.lines.push_back("protective MBR: 0x55AA signature, 0xEE"
                                " entry present");

        /* ---- 2. Primary GPT ------------------------------------------ */
        uint8_t hdr[92];
        preadAll(fd, SECTOR, hdr, sizeof(hdr));
        if (std::memcmp(hdr, "EFI PART", 8) != 0) {
            rep.problems.push_back("GPT: \"EFI PART\" header missing at"
                                   " LBA 1 - the disk is not partitioned"
                                   " with GPT (the PrismOS installer only"
                                   " produces GPT)");
            return rep;
        }

        uint32_t declaredCrc = le32(hdr + 16);
        {
            std::vector<uint8_t> tmp(hdr, hdr + 92);
            std::memset(tmp.data() + 16, 0, 4);
            if (util::crc32(tmp.data(), tmp.size()) != declaredCrc)
                rep.problems.push_back("GPT: primary header CRC"
                                       " incorrect - the firmware will ignore"
                                       " the table");
        }

        uint64_t myLba = le64(hdr + 24), altLba = le64(hdr + 32);
        uint64_t firstUsable = le64(hdr + 40), lastUsable = le64(hdr + 48);
        uint64_t entriesLba = le64(hdr + 72);
        uint32_t nEntries = le32(hdr + 80), entrySize = le32(hdr + 84);
        uint32_t entriesCrc = le32(hdr + 88);

        if (myLba != 1)
            rep.problems.push_back("GPT: MyLBA = " + std::to_string(myLba)
                                   + " (expected 1)");
        if (altLba != totalSectors - 1)
            rep.lines.push_back("GPT: backup header announced at LBA "
                                + std::to_string(altLba) + " (end of disk: "
                                + std::to_string(totalSectors - 1) + ")");

        if (entrySize < 128 || entrySize > 512 || nEntries == 0
            || nEntries > 1024) {
            rep.problems.push_back("GPT: invalid entry array"
                                   " geometry");
            return rep;
        }

        std::vector<uint8_t> entries(size_t(nEntries) * entrySize);
        preadAll(fd, entriesLba * SECTOR, entries.data(), entries.size());
        if (util::crc32(entries.data(), entries.size()) != entriesCrc)
            rep.problems.push_back("GPT: entry array CRC incorrect"
                                   " - the firmware will ignore the partitions");

        /* ---- Partitions ---------------------------------------------- */
        std::vector<PartitionEntry> parts;
        std::vector<uint8_t> espType = util::Uuid::parse(GUID_ESP).bytesLe();
        std::vector<uint8_t> biosType =
            util::Uuid::parse(GUID_BIOS_BOOT).bytesLe();

        for (uint32_t i = 0; i < nEntries; ++i) {
            const uint8_t* e = entries.data() + size_t(i) * entrySize;
            bool allZero = true;
            for (int k = 0; k < 16; ++k)
                if (e[k]) { allZero = false; break; }
            if (allZero) continue;
            PartitionEntry p;
            p.number = int(i) + 1;
            p.typeGuid = guidToString(e);
            p.name = decodeName(e + 56, size_t(entrySize) - 56);
            p.first = le64(e + 32);
            p.last = le64(e + 40);
            p.attrs = le64(e + 48);
            parts.push_back(p);

            if (p.first > p.last || p.first < firstUsable
                || p.last > lastUsable)
                rep.problems.push_back("GPT: partition " + std::to_string(p.number)
                                       + " out of usable range ("
                                       + std::to_string(p.first) + ".."
                                       + std::to_string(p.last) + ")");
            rep.lines.push_back("P" + std::to_string(p.number) + " \""
                                + p.name + "\": LBA "
                                + std::to_string(p.first) + ", "
                                + humanSize((p.last - p.first + 1) * SECTOR)
                                + " - type " + p.typeGuid);
        }
        if (parts.empty())
            rep.problems.push_back("GPT: no partitions defined");

        /* ---- Backup GPT ---------------------------------------------- */
        try {
            std::vector<uint8_t> bkEntries(entries.size());
            preadAll(fd, (altLba - (size_t(entries.size()) / SECTOR)) * SECTOR,
                     bkEntries.data(), bkEntries.size());
            if (bkEntries != entries)
                rep.problems.push_back("GPT: backup entries"
                                       " differ from the primaries");
            uint8_t bkHdr[92];
            preadAll(fd, altLba * SECTOR, bkHdr, sizeof(bkHdr));
            if (std::memcmp(bkHdr, "EFI PART", 8) != 0)
                rep.problems.push_back("GPT: backup header"
                                       " unreadable");
            else {
                uint32_t bkCrc = le32(bkHdr + 16);
                std::vector<uint8_t> tmp(bkHdr, bkHdr + 92);
                std::memset(tmp.data() + 16, 0, 4);
                if (util::crc32(tmp.data(), tmp.size()) != bkCrc)
                    rep.problems.push_back("GPT: backup header"
                                           " CRC incorrect");
                else
                    rep.lines.push_back("GPT: backup header and entries"
                                        " compliant (CRC OK)");
            }
        } catch (const std::exception& ex) {
            rep.problems.push_back(std::string("GPT: backup unreadable: ")
                                   + ex.what());
        }

        /* ---- 3+4. ESP FAT32 + UEFI boot ------------------------------- */
        const PartitionEntry* esp = nullptr;
        const PartitionEntry* bios = nullptr;
        for (const auto& p : parts) {
            if (!esp && p.typeGuid == guidToString(espType.data())) esp = &p;
            if (!bios && p.typeGuid == guidToString(biosType.data())) bios = &p;
        }
        if (!esp) {
            rep.problems.push_back("UEFI: no EFI System Partition"
                                   " (type GUID C12A7328-...) - the UEFI"
                                   " firmware will not look for a boot"
                                   " file on this disk");
        } else {
            rep.lines.push_back("ESP found: P" + std::to_string(esp->number)
                                + ", FAT32 expected starting at LBA "
                                + std::to_string(esp->first));
            /* ESP position: the FIRST partition is where some OEM
             * firmwares only look for the default boot path.  USB drives
             * installed with the old layout (BIOS boot partition first,
             * before 2026-10-18) carry their ESP as P2: technically
             * bootable on conformant firmwares, but invisible on those
             * OEM machines. */
            if (esp->number != 1)
                rep.lines.push_back(
                    "note: the ESP is partition P"
                    + std::to_string(esp->number)
                    + ", not P1 - some OEM firmwares only try the default"
                      " boot path (EFI/BOOT/BOOTX64.EFI) on the FIRST"
                      " partition of a removable medium; reinstall with"
                      " the 2026-10-18 (night) version (ESP first) for"
                      " maximum compatibility");
            /* 'legacy BIOS bootable' attribute (bit 2): optional for
             * compliant firmwares, but some OEM BIOSes only list a GPT
             * disk in the boot menu if it is set.  USB drives installed
             * before 2026-10-18 (evening) do not have it. */
            rep.lines.push_back(
                std::string("ESP: 'legacy BIOS bootable' attribute ")
                + ((esp->attrs & 0x4)
                   ? "set (maximum visibility in boot menus)"
                   : "missing - optional, but some firmwares only "
                     "list the USB drive in the F12 menu if it is set;"
                     " reinstall with the 2026-10-18 (evening) version"
                     " to add it"));
            if (esp->first % 2048 != 0)
                rep.lines.push_back("ESP: start not aligned to MiB (LBA "
                                    + std::to_string(esp->first)
                                    + ") - works, but suboptimal");

            auto audit = verify::auditEsp(fd, esp->first * SECTOR);
            for (const auto& p : audit.problems)
                rep.problems.push_back(p);
            if (audit.readable) {
                rep.lines.push_back(
                    "ESP FAT32: label \"" + audit.volumeLabel
                    + "\", clusters of "
                    + std::to_string(audit.sectorsPerCluster * 512)
                    + " B, " + std::to_string(audit.usedClusters) + "/"
                    + std::to_string(audit.clusterCount) + " clusters"
                    " used");
                rep.lines.push_back("ESP: " + std::to_string(audit.files)
                                    + " files, "
                                    + std::to_string(audit.dirs)
                                    + " subdirectories ("
                                    + humanSize(audit.payloadBytes)
                                    + "), all re-read via cluster"
                                      " chains");
            }
            if (audit.hasBootx64)
                rep.lines.push_back("UEFI: EFI/BOOT/BOOTX64.EFI present - "
                                    + audit.bootx64Detail);
        }

        /* ---- 5. BIOS boot partition ------------------------------------ */
        if (!bios) {
            rep.lines.push_back("BIOS: no BIOS boot partition -"
                                " legacy (CSM) boot is not"
                                " installed on this disk");
        } else {
            try {
                uint8_t b1[512];
                preadAll(fd, bios->first * SECTOR, b1, sizeof(b1));
                if (le32(b1) == BOOT1_MAGIC) {
                    uint64_t loaderLba = le64(b1 + 4);
                    uint32_t loaderSectors = le32(b1 + 12);
                    rep.lines.push_back(
                        "BIOS: boot1 present in P"
                        + std::to_string(bios->number)
                        + " (magic PRB1), bootloader @LBA "
                        + std::to_string(loaderLba) + " ("
                        + std::to_string(loaderSectors) + " sectors)");
                } else {
                    rep.problems.push_back(
                        "BIOS: \"PRB1\" magic missing at the start of P"
                        + std::to_string(bios->number)
                        + " - BIOS boot will not work");
                }
            } catch (const std::exception& ex) {
                rep.problems.push_back(std::string("BIOS: boot partition "
                                                   "unreadable: ")
                                       + ex.what());
            }
        }
    } catch (const std::exception& ex) {
        rep.problems.push_back(std::string("audit interrupted: ")
                               + ex.what());
    }

    return rep;
}

} // namespace prism::check
