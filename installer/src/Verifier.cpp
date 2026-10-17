/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - verifier implementation.
 */

#include "Verifier.h"
#include "Pe.h"
#include "Util.h"

#include <algorithm>
#include <cstring>
#include <filesystem>
#include <map>
#include <set>

#include <unistd.h>

namespace fs = std::filesystem;

namespace prism::verify {

namespace {

/* ---- Read primitives ------------------------------------------------ */

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
    return uint32_t(p[0]) | (uint32_t(p[1]) << 8) |
           (uint32_t(p[2]) << 16) | (uint32_t(p[3]) << 24);
}

uint64_t le64(const uint8_t* p)
{
    return uint64_t(le32(p)) | (uint64_t(le32(p + 4)) << 32);
}

std::string lower(std::string s)
{
    for (auto& c : s) c = char(std::tolower((unsigned char)c));
    return s;
}

/* ---- FAT32 reader (independent of the writer) ------------------------ */

struct FatVolume {
    int fd;
    uint64_t base;

    uint16_t bytesPerSector = 0;
    uint8_t  sectorsPerCluster = 0;
    uint16_t reservedSectors = 0;
    uint32_t fatSectors = 0;
    uint32_t clusterCount = 0;
    uint32_t rootCluster = 0;
    uint64_t fatOffset = 0;       /* offset of FAT #1 in the volume    */
    uint64_t dataOffset = 0;      /* start of cluster area             */

    std::vector<uint8_t> fat;

    explicit FatVolume(int f, uint64_t b) : fd(f), base(b)
    {
        uint8_t bs[512];
        preadAll(fd, base, bs, sizeof(bs));

        if (bs[0x1FE] != 0x55 || bs[0x1FF] != 0xAA)
            throw std::runtime_error("0x55AA signature missing from the "
                                     "FAT32 boot sector");
        if (std::memcmp(bs + 0x52, "FAT32   ", 8) != 0)
            throw std::runtime_error("unexpected FAT type in the boot "
                                     "sector (expected: FAT32)");

        bytesPerSector  = le16(bs + 0x0B);
        sectorsPerCluster = bs[0x0D];
        reservedSectors = le16(bs + 0x0E);
        fatSectors      = le32(bs + 0x24);
        rootCluster     = le32(bs + 0x2C);

        if (bytesPerSector != 512)
            throw std::runtime_error("BPB: unexpected sector size");

        fatOffset = uint64_t(reservedSectors) * bytesPerSector;
        dataOffset = fatOffset + 2ull * fatSectors * bytesPerSector;
        /* data clusters (cluster #2 = first) */
        clusterCount = 0;
        uint32_t total = le32(bs + 0x20);
        if (total == 0)
            throw std::runtime_error("BPB: TotSec32 is zero");

        fat.assign(size_t(fatSectors) * bytesPerSector, 0);
        preadAll(fd, base + fatOffset, fat.data(), fat.size());

        clusterCount = (total - reservedSectors - 2 * fatSectors)
                       / sectorsPerCluster;
        if (clusterCount < 65525)
            throw std::runtime_error("BPB: invalid FAT32 cluster "
                                     "count (< 65525)");
    }

    uint32_t fatEntry(uint32_t cluster) const
    {
        return le32(fat.data() + cluster * 4) & 0x0FFFFFFF;
    }

    /* Returns the list of clusters of the chain starting at c. */
    std::vector<uint32_t> chain(uint32_t c) const
    {
        std::vector<uint32_t> out;
        std::set<uint32_t> seen;
        uint32_t cur = c;
        while (cur >= 2 && cur < clusterCount + 2) {
            if (!seen.insert(cur).second)
                throw std::runtime_error("loop detected in the FAT "
                                         "(cluster " + std::to_string(cur) + ")");
            out.push_back(cur);
            uint32_t next = fatEntry(cur);
            if (next >= 0x0FFFFFF8) break;      /* EOC */
            cur = next;
        }
        if (out.empty() || fatEntry(out.back()) < 0x0FFFFFF8)
            throw std::runtime_error("cluster chain not terminated "
                                     "(cluster " + std::to_string(c) + ")");
        return out;
    }

    /* Reads the bytes of a cluster chain. */
    std::vector<uint8_t> readChain(uint32_t c, uint64_t size)
    {
        auto clusters = chain(c);
        uint64_t cs = uint64_t(sectorsPerCluster) * bytesPerSector;
        std::vector<uint8_t> out;
        out.reserve(size_t(size));
        for (uint32_t cl : clusters) {
            uint64_t off = dataOffset
                           + uint64_t(cl - 2) * cs;
            size_t take = size_t(std::min<uint64_t>(cs, size - out.size()));
            std::vector<uint8_t> buf(take);
            preadAll(fd, base + off, buf.data(), take);
            out.insert(out.end(), buf.begin(), buf.end());
            if (out.size() >= size) break;
        }
        if (out.size() != size)
            throw std::runtime_error("chain too short for the file's "
                                     "declared size");
        return out;
    }
};

struct Entry {
    std::string name;          /* exact long name (or reconstructed 8.3) */
    bool isDir = false;
    uint32_t firstCluster = 0;
    uint32_t size = 0;
};

/* Reconstructs an 8.3 name from its 11 bytes. */
std::string shortNameToString(const uint8_t sn[11])
{
    std::string base(reinterpret_cast<const char*>(sn), 8);
    std::string ext(reinterpret_cast<const char*>(sn + 8), 3);
    auto rtrim = [](std::string& s) {
        s.erase(s.find_last_not_of(' ') + 1);
    };
    rtrim(base);
    rtrim(ext);
    return ext.empty() ? base : base + "." + ext;
}

/* LFN checksum of an 8.3 name (Microsoft algorithm, implemented
 * independently of the writer: any mismatch indicates a lost write or
 * orphan entries - a compliant firmware then ignores the long name,
 * which changes the file seen by the bootloader). */
uint8_t lfnChecksum(const uint8_t sn[11])
{
    uint8_t sum = 0;
    for (int i = 0; i < 11; ++i)
        sum = uint8_t(((sum & 1) << 7) + (sum >> 1) + sn[i]);
    return sum;
}

/* Lists the entries of a directory (walks its cluster chain). */
std::vector<Entry> listDir(FatVolume& vol, uint32_t firstCluster)
{
    auto clusters = vol.chain(firstCluster);
    uint64_t cs = uint64_t(vol.sectorsPerCluster) * vol.bytesPerSector;

    std::vector<uint8_t> data;
    for (uint32_t cl : clusters) {
        std::vector<uint8_t> buf;
        buf.resize(size_t(cs));
        preadAll(vol.fd, vol.base + vol.dataOffset
                  + uint64_t(cl - 2) * cs, buf.data(), buf.size());
        data.insert(data.end(), buf.begin(), buf.end());
    }

    std::vector<Entry> out;
    std::string pendingLfn;
    uint16_t pendingSeq = 0;
    uint8_t pendingCsum = 0;      /* LFN checksum of the pending set */

    /* Diagnostic context: first suspicious LFN entry. */
    auto hexDump = [](const uint8_t* e) {
        static const char* digits = "0123456789ABCDEF";
        std::string s;
        for (int i = 0; i < 32; ++i) {
            s += digits[e[i] >> 4];
            s += digits[e[i] & 0xF];
            if (i != 31) s.push_back(' ');
        }
        return s;
    };
    std::string badCtx;

    for (size_t off = 0; off + 32 <= data.size(); off += 32) {
        const uint8_t* e = data.data() + off;
        if (e[0] == 0x00) break;                       /* end of directory */
        if (e[0] == 0xE5) { pendingLfn.clear(); continue; }  /* deleted */

        uint8_t attr = e[0x0B];
        if (attr == 0x0F) {
            /* LFN entry: accumulate (descending order). */
            uint8_t seq = e[0];
            uint8_t csum = e[0x0D];
            std::string utf16;
            for (int k = 0; k < 5; ++k) utf16 += std::string(
                reinterpret_cast<const char*>(e + 1 + k * 2), 2);
            for (int k = 0; k < 6; ++k) utf16 += std::string(
                reinterpret_cast<const char*>(e + 0x0E + k * 2), 2);
            for (int k = 0; k < 2; ++k) utf16 += std::string(
                reinterpret_cast<const char*>(e + 0x1C + k * 2), 2);

            std::string name;
            bool nonAscii = false;
            for (size_t i = 0; i + 1 < utf16.size(); i += 2) {
                uint16_t ch = uint8_t(utf16[i]) | (uint8_t(utf16[i + 1]) << 8);
                if (ch == 0x0000) break;
                if (ch == 0xFFFF) continue;
                if (ch > 0x7F) {
                    nonAscii = true;      /* detailed after the loop */
                    continue;
                }
                name += char(ch);
            }
            if (nonAscii && badCtx.empty())
                badCtx = "32-byte entry @+" + std::to_string(off)
                         + ": " + hexDump(e);

            if (seq & 0x40) {
                pendingLfn = name;
                pendingSeq = seq & 0x3F;
                pendingCsum = csum;
            } else if (pendingSeq == (seq & 0x3F) + 1 && pendingCsum == csum) {
                pendingLfn = name + pendingLfn;
                pendingSeq = seq & 0x3F;
            } else {
                pendingLfn.clear();
            }
            continue;
        }

        if (attr & 0x08) { pendingLfn.clear(); continue; }  /* volume label */

        Entry entry;
        entry.isDir = (attr & 0x10) != 0;
        entry.firstCluster = uint32_t(le16(e + 0x1A))
                             | (uint32_t(le16(e + 0x14)) << 16);
        entry.size = le32(e + 0x1C);
        entry.name = pendingLfn.empty() ? shortNameToString(e) : pendingLfn;

        /* An LFN set must match its short entry: a real firmware
         * (EDK2) checks this sum and IGNORES the long name on a
         * mismatch - the file then becomes visible under its 8.3
         * alias only.  A mismatch here signals a lost write or
         * residual bytes. */
        if (!pendingLfn.empty() && lfnChecksum(e) != pendingCsum)
            throw std::runtime_error(
                "LFN: incorrect checksum for \"" + pendingLfn
                + "\" (cluster " + std::to_string(firstCluster) + ")");
        pendingLfn.clear();

        if (entry.name == "." || entry.name == "..") continue;
        out.push_back(std::move(entry));
    }

    /* A non-ASCII LFN means directory bytes that are not the expected
     * ones (failing media, lost write...): the hexadecimal context is
     * kept for diagnosis. */
    if (!badCtx.empty())
        throw std::runtime_error("unexpected non-ASCII LFN (cluster "
                                 + std::to_string(firstCluster) + ", "
                                 + badCtx + ")");
    return out;
}

} // namespace

/* ---- ESP verification ------------------------------------------------ */

std::vector<std::string> verifyEsp(int fd, uint64_t baseOffset,
                                   const std::string& srcDir,
                                   const std::function<void(const std::string&)>& onFile)
{
    std::vector<std::string> problems;

    try {
        FatVolume vol(fd, baseOffset);

        /* FSInfo / FAT consistency. */
        uint8_t info[512];
        preadAll(fd, baseOffset + 512, info, sizeof(info));
        if (le32(info + 0x000) != 0x41615252)
            problems.push_back("FSInfo: RRaA signature missing");
        uint32_t declaredFree = le32(info + 0x1E8);
        uint32_t actualFree = 0;
        for (uint32_t c = 2; c < vol.clusterCount + 2; ++c)
            if (vol.fatEntry(c) == 0) actualFree += 1;
        if (declaredFree != 0xFFFFFFFFu && declaredFree != actualFree)
            problems.push_back("FSInfo: " + std::to_string(declaredFree)
                               + " free clusters declared, "
                               + std::to_string(actualFree) + " actual");

        /* Recursive walk + file comparison. */
        std::function<void(uint32_t, const fs::path&, const std::string&)>
        walk = [&](uint32_t cluster, const fs::path& diskDir,
                   const std::string& rel) {
            auto entries = listDir(vol, cluster);
            std::map<std::string, const Entry*> byName;
            for (const auto& e : entries)
                byName[lower(e.name)] = &e;

            /* Every entry must exist on the source side. */
            for (const auto& e : entries) {
                fs::path expected = diskDir / e.name;
                if (!fs::exists(expected))
                    problems.push_back("file present on the ESP but missing "
                                       "from the source: " + rel + e.name);
            }

            /* Every source file/directory must be on the ESP. */
            for (const auto& de : fs::directory_iterator(diskDir)) {
                std::string name = de.path().filename().string();
                if (name == "." || name == "..") continue;
                auto it = byName.find(lower(name));
                if (it == byName.end()) {
                    problems.push_back("file missing on the ESP: "
                                       + rel + name);
                    continue;
                }
                const Entry& e = *it->second;

                if (de.is_directory()) {
                    if (!e.isDir)
                        problems.push_back("unexpected type (file instead "
                                           "of directory): " + rel + name);
                    else
                        walk(e.firstCluster, de.path(),
                             rel + name + "/");
                } else if (de.is_regular_file()) {
                    if (e.isDir) {
                        problems.push_back("unexpected type (directory instead "
                                           "of file): " + rel + name);
                        continue;
                    }
                    auto expected = util::readFile(de.path().string());
                    if (e.size != expected.size()) {
                        problems.push_back("incorrect size: " + rel + name
                                           + " (" + std::to_string(e.size)
                                           + " on disk, "
                                           + std::to_string(expected.size())
                                           + " expected)");
                        continue;
                    }
                    auto got = vol.readChain(e.firstCluster, e.size);
                    if (util::crc32(got.data(), got.size())
                        != util::crc32(expected.data(), expected.size())) {
                        problems.push_back("different content (CRC): "
                                           + rel + name);
                    }
                    if (onFile) onFile(rel + name);
                }
            }
        };

        walk(vol.rootCluster, fs::path(srcDir), "");
    } catch (const std::exception& ex) {
        problems.push_back(std::string("ESP unreadable: ") + ex.what());
    }
    return problems;
}

/* ---- Standalone audit (no source) ------------------------------------ */

namespace {

/* Resolves a case-insensitive path from the root of the volume
 * ("EFI/BOOT/BOOTX64.EFI").  Returns false if an element is missing. */
bool resolvePath(FatVolume& vol, const std::string& path, Entry* out)
{
    std::vector<std::string> parts;
    std::string cur;
    for (char c : path) {
        if (c == '/') {
            if (!cur.empty()) { parts.push_back(cur); cur.clear(); }
        } else {
            cur.push_back(c);
        }
    }
    if (!cur.empty()) parts.push_back(cur);
    if (parts.empty()) return false;

    uint32_t cluster = vol.rootCluster;
    Entry found{};
    for (size_t i = 0; i < parts.size(); ++i) {
        auto entries = listDir(vol, cluster);
        const Entry* hit = nullptr;
        for (const auto& e : entries) {
            if (lower(e.name) == lower(parts[i])) { hit = &e; break; }
        }
        if (!hit) return false;
        if (i + 1 == parts.size()) { found = *hit; break; }
        if (!hit->isDir) return false;
        cluster = hit->firstCluster;
    }
    if (out) *out = found;
    return true;
}

/* FAT32 backup sectors: sector 6 must reproduce the boot sector,
 * sector 7 the FSInfo.  A firmware that re-reads these copies (some
 * do so after a reset) must find the same geometry there as in the
 * primary. */
void checkBackupSectors(int fd, uint64_t baseOffset,
                        std::vector<std::string>& problems)
{
    uint8_t prim[512], backup[512], infoPrim[512], infoBackup[512];
    try {
        preadAll(fd, baseOffset, prim, sizeof(prim));
        preadAll(fd, baseOffset + 6 * 512, backup, sizeof(backup));
        preadAll(fd, baseOffset + 1 * 512, infoPrim, sizeof(infoPrim));
        preadAll(fd, baseOffset + 7 * 512, infoBackup, sizeof(infoBackup));
    } catch (const std::exception& ex) {
        problems.push_back(std::string("FAT32 backup sectors "
                                       "unreadable: ") + ex.what());
        return;
    }
    if (std::memcmp(prim, backup, 512) != 0)
        problems.push_back("FAT32: backup boot sector (6) "
                           "differs from the primary");
    if (std::memcmp(infoPrim, infoBackup, 512) != 0)
        problems.push_back("FAT32: backup FSInfo (7) differs from the "
                           "primary");
}

/* Volume label (BS_VolLab, 11 bytes padded with spaces). */
std::string readVolumeLabel(int fd, uint64_t baseOffset)
{
    uint8_t bs[512];
    preadAll(fd, baseOffset, bs, sizeof(bs));
    std::string label(reinterpret_cast<const char*>(bs + 0x47), 11);
    label.erase(label.find_last_not_of(' ') + 1);
    return label;
}

} // namespace

EspAudit auditEsp(int fd, uint64_t baseOffset)
{
    EspAudit audit;
    try {
        FatVolume vol(fd, baseOffset);
        audit.readable = true;
        audit.sectorsPerCluster = vol.sectorsPerCluster;
        audit.clusterCount = vol.clusterCount;
        audit.volumeLabel = readVolumeLabel(fd, baseOffset);

        /* Backup sectors. */
        checkBackupSectors(fd, baseOffset, audit.problems);

        /* Cluster counting + FSInfo consistency. */
        uint32_t freeClusters = 0;
        for (uint32_t c = 2; c < vol.clusterCount + 2; ++c)
            if (vol.fatEntry(c) == 0) freeClusters += 1;
        audit.freeClusters = freeClusters;
        audit.usedClusters = vol.clusterCount - freeClusters;

        uint8_t info[512];
        preadAll(fd, baseOffset + 512, info, sizeof(info));
        if (le32(info + 0x000) != 0x41615252)
            audit.problems.push_back("FSInfo: RRaA signature missing");
        uint32_t declaredFree = le32(info + 0x1E8);
        if (declaredFree != 0xFFFFFFFFu && declaredFree != freeClusters)
            audit.problems.push_back("FSInfo: "
                                     + std::to_string(declaredFree)
                                     + " free clusters declared, "
                                     + std::to_string(freeClusters)
                                     + " actual");

        /* Full walk: every file must be readable by following its
         * cluster chain (re-reads the media entirely). */
        std::set<uint32_t> visitedDirs;
        std::function<void(uint32_t, const std::string&)> walk =
            [&](uint32_t cluster, const std::string& rel) {
                if (!visitedDirs.insert(cluster).second) {
                    audit.problems.push_back("directory visited twice "
                                             "(loop): " + rel);
                    return;
                }
                auto entries = listDir(vol, cluster);
                for (const auto& e : entries) {
                    if (e.isDir) {
                        audit.dirs += 1;
                        walk(e.firstCluster, rel + e.name + "/");
                        continue;
                    }
                    audit.files += 1;
                    audit.payloadBytes += e.size;
                    try {
                        vol.readChain(e.firstCluster, e.size);
                    } catch (const std::exception& ex) {
                        audit.problems.push_back(
                            "unreadable file: " + rel + e.name
                            + " (" + ex.what() + ")");
                    }
                }
            };
        walk(vol.rootCluster, "");

        /* UEFI boot path: EFI/BOOT/BOOTX64.EFI + PE/COFF. */
        Entry efi{};
        if (!resolvePath(vol, "EFI/BOOT/BOOTX64.EFI", &efi)) {
            audit.problems.push_back(
                "UEFI: EFI/BOOT/BOOTX64.EFI is missing from the ESP - the "
                "UEFI firmware has nothing to boot on this disk "
                "(rebuild the tree with \"make\" at the root "
                "of PrismOS)");
        } else if (efi.isDir) {
            audit.problems.push_back("UEFI: EFI/BOOT/BOOTX64.EFI is a "
                                     "directory, not a file");
        } else {
            audit.hasBootx64 = true;
            audit.bootx64Bytes = efi.size;
            try {
                auto bytes = vol.readChain(efi.firstCluster, efi.size);
                std::string peErr = pe::validateEfiAppBytes(bytes.data(),
                                                            bytes.size());
                if (!peErr.empty()) {
                    audit.problems.push_back("UEFI: EFI/BOOT/BOOTX64.EFI: "
                                             + peErr);
                    audit.bootx64Detail = peErr;
                } else {
                    audit.bootx64Detail =
                        "valid x86-64 EFI application ("
                        + std::to_string(efi.size) + " bytes)";
                }
            } catch (const std::exception& ex) {
                audit.problems.push_back(
                    std::string("UEFI: EFI/BOOT/BOOTX64.EFI unreadable (")
                    + ex.what() + ")");
                audit.bootx64Detail = ex.what();
            }
        }
    } catch (const std::exception& ex) {
        audit.problems.push_back(std::string("ESP unreadable: ") + ex.what());
    }
    return audit;
}

std::vector<std::string> verifyUefiBoot(int fd, uint64_t baseOffset)
{
    /* Targeted variant (post-installation): the full file check is
     * done by verifyEsp; what is checked here is what no firmware
     * forgives - the absence or invalidity of the boot file. */
    std::vector<std::string> problems;
    try {
        FatVolume vol(fd, baseOffset);
        checkBackupSectors(fd, baseOffset, problems);

        Entry efi{};
        if (!resolvePath(vol, "EFI/BOOT/BOOTX64.EFI", &efi)) {
            problems.push_back(
                "UEFI: EFI/BOOT/BOOTX64.EFI is missing from the re-read ESP - "
                "the disk would NOT boot in UEFI");
        } else if (!efi.isDir) {
            auto bytes = vol.readChain(efi.firstCluster, efi.size);
            std::string peErr = pe::validateEfiAppBytes(bytes.data(),
                                                        bytes.size());
            if (!peErr.empty())
                problems.push_back("UEFI: EFI/BOOT/BOOTX64.EFI re-read "
                                   "from the disk: " + peErr);
        } else {
            problems.push_back("UEFI: EFI/BOOT/BOOTX64.EFI is a "
                               "directory, not a file");
        }
    } catch (const std::exception& ex) {
        problems.push_back(std::string("UEFI: ESP unreadable: ")
                           + ex.what());
    }
    return problems;
}

/* ---- GPT / MBR / boot verification ------------------------------------- */

std::vector<std::string> verifyGptBoot(int fd,
                                       const std::vector<uint8_t>& expectedMbr,
                                       const std::vector<uint8_t>& expectedEntries,
                                       uint64_t totalSectors,
                                       uint64_t bootStart,
                                       const std::vector<uint8_t>& expectedBootPart)
{
    std::vector<std::string> problems;

    auto expect = [&](bool ok, const std::string& what) {
        if (!ok) problems.push_back(what);
    };

    try {
        /* Protective MBR. */
        std::vector<uint8_t> mbr(512);
        preadAll(fd, 0, mbr.data(), mbr.size());
        expect(mbr[0x1FE] == 0x55 && mbr[0x1FF] == 0xAA,
               "MBR: 0x55AA signature missing");
        expect(mbr[0x1C2] == 0xEE, "MBR: protective 0xEE entry missing");
        expect(mbr == expectedMbr, "MBR: bytes differ from expected");

        /* Primary GPT header. */
        std::vector<uint8_t> hdr(92);
        preadAll(fd, 512, hdr.data(), hdr.size());
        expect(std::memcmp(hdr.data(), "EFI PART", 8) == 0,
               "GPT: EFI PART signature missing");
        uint32_t declaredCrc = le32(hdr.data() + 16);
        std::vector<uint8_t> tmp = hdr;
        std::memset(tmp.data() + 16, 0, 4);
        expect(util::crc32(tmp.data(), 92) == declaredCrc,
               "GPT: primary header CRC incorrect");
        expect(le64(hdr.data() + 24) == 1, "GPT: MyLBA != 1");
        expect(le64(hdr.data() + 32) == totalSectors - 1,
               "GPT: AltLBA incorrect");

        /* Primary entries. */
        std::vector<uint8_t> entries(128 * 128);
        preadAll(fd, 2 * 512, entries.data(), entries.size());
        expect(le32(hdr.data() + 88) == util::crc32(entries.data(), entries.size()),
               "GPT: entry array CRC incorrect");
        expect(entries == expectedEntries,
               "GPT: entries differ from expected");

        /* Backup. */
        std::vector<uint8_t> bkEntries(128 * 128);
        preadAll(fd, (totalSectors - 33) * 512, bkEntries.data(),
                 bkEntries.size());
        expect(bkEntries == expectedEntries,
               "GPT: backup entries differ");
        std::vector<uint8_t> bkHdr(92);
        preadAll(fd, (totalSectors - 1) * 512, bkHdr.data(), bkHdr.size());
        expect(std::memcmp(bkHdr.data(), "EFI PART", 8) == 0,
               "GPT: backup header unreadable");
        uint32_t bkCrc = le32(bkHdr.data() + 16);
        std::vector<uint8_t> bkTmp = bkHdr;
        std::memset(bkTmp.data() + 16, 0, 4);
        expect(util::crc32(bkTmp.data(), 92) == bkCrc,
               "GPT: backup header CRC incorrect");
        expect(le64(bkHdr.data() + 24) == totalSectors - 1,
               "GPT: backup MyLBA incorrect");

        /* Boot partition: exact bytes. */
        std::vector<uint8_t> boot(expectedBootPart.size());
        preadAll(fd, bootStart * 512, boot.data(), boot.size());
        expect(boot == expectedBootPart,
               "boot partition: content differs from expected");
    } catch (const std::exception& ex) {
        problems.push_back(std::string("unable to re-read: ")
                           + ex.what());
    }
    return problems;
}

} // namespace prism::verify
