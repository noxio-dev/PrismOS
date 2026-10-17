/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - FAT32 writer implementation (streaming).
 */

#include "Fat32.h"
#include "Util.h"

#include <algorithm>
#include <array>
#include <cctype>
#include <cstring>
#include <filesystem>
#include <functional>
#include <set>
#include <stdexcept>
#include <sys/stat.h>
#include <unistd.h>

namespace fs = std::filesystem;

namespace prism::fat32 {

namespace {

constexpr uint16_t BYTES_PER_SECTOR = 512;
constexpr uint16_t RESERVED_SECTORS  = 32;     /* standard FAT32        */
constexpr uint8_t  NUM_FATS          = 2;
constexpr uint32_t ROOT_CLUSTER      = 2;
constexpr uint16_t FSINFO_SECTOR     = 1;
constexpr uint16_t BACKUP_BOOT_SECTOR = 6;

constexpr uint32_t MIN_CLUSTERS = 65525;        /* FAT32 floor (spec)    */
constexpr uint32_t FAT_EOC      = 0x0FFFFFF8;

void put16(uint8_t* p, uint16_t v)
{
    p[0] = uint8_t(v); p[1] = uint8_t(v >> 8);
}

void put32(uint8_t* p, uint32_t v)
{
    p[0] = uint8_t(v); p[1] = uint8_t(v >> 8);
    p[2] = uint8_t(v >> 16); p[3] = uint8_t(v >> 24);
}

/* ------------------------------------------------------------------ */
/* pwrite writes onto the target descriptor.                            */
/* ------------------------------------------------------------------ */

class Sink {
public:
    Sink(int fd, uint64_t base) : fd_(fd), base_(base) {}

    /* Absolute offset of the volume within the descriptor (start of
     * the partition) - feeds the BPB_HiddSec field. */
    uint64_t base() const { return base_; }

    /* Writes len bytes at relative offset off of the volume. */
    void write(uint64_t off, const void* data, size_t len)
    {
        const uint8_t* p = static_cast<const uint8_t*>(data);
        while (len > 0) {
            ssize_t n = ::pwrite(fd_, p, len, off_t(base_ + off));
            if (n < 0) {
                if (errno == EINTR) continue;
                throw std::runtime_error(
                    std::string("ESP write failed (pwrite): ")
                    + std::strerror(errno));
            }
            off += uint64_t(n);
            p += n;
            len -= size_t(n);
        }
    }

private:
    int fd_;
    uint64_t base_;
};

/* ------------------------------------------------------------------ */
/* FAT32 volume under construction.                                     */
/* ------------------------------------------------------------------ */

class Volume {
public:
    Volume(Sink& sink, uint64_t volumeBytes, const std::string& label)
        : sink_(sink), volumeBytes_(volumeBytes), label_(label)
    {
        computeGeometry(volumeBytes);

        /* "Hidden" sectors (BPB_HiddSec) = LBA of the start of the
         * partition, exactly like mkfs.fat formatting a real
         * partition - some repair tools cross-check this field
         * against the partition table. */
        hiddenSectors_ = uint32_t(sink_.base() / BYTES_PER_SECTOR);

        /* FATs kept in memory (modest size), written at the end. */
        fat_.assign(size_t(fatSectors_) * BYTES_PER_SECTOR, 0);
        put32(fat_.data() + 0, 0x0FFFFFF8);      /* media        */
        put32(fat_.data() + 4, 0x0FFFFFFF);      /* EOC/state    */

        /* Reserved sectors (2..31) explicitly zeroed: on a reused
         * medium - already-booted key, recycled image - old bytes
         * would otherwise remain there (a NEW image is zeros
         * everywhere, a real disk is not).  Backup sectors 6
         * (BPB) and 7 (FSInfo) are rewritten right after. */
        {
            std::vector<uint8_t> zeroSector(BYTES_PER_SECTOR, 0);
            for (uint32_t sec = 2; sec < RESERVED_SECTORS; ++sec)
                sink_.write(uint64_t(sec) * BYTES_PER_SECTOR,
                            zeroSector.data(), zeroSector.size());
        }

        writeBootSector(0);
        writeBootSector(BACKUP_BOOT_SECTOR);
        writeFsInfo(FSINFO_SECTOR);
        writeFsInfo(BACKUP_BOOT_SECTOR + 1);

        /* NB: cluster 2 (root directory) is allocated by the
         * first allocation of copyDir - the root MUST be the
         * first data cluster, as the BPB announces
         * (BPB_RootClus = 2). */
    }

    const std::string& label() const { return label_; }
    uint32_t sectorsPerCluster() const { return spc_; }
    uint32_t clusterCount() const { return clusterCount_; }
    uint32_t usedClusters() const { return nextFree_ - ROOT_CLUSTER; }

    /* Allocates a cluster (marked EOC in the FAT). */
    uint32_t allocCluster()
    {
        if (nextFree_ - ROOT_CLUSTER >= clusterCount_)
            throw std::runtime_error(
                "the ESP is full: no free clusters left "
                "(increase the ESP size)");
        uint32_t c = nextFree_++;
        put32(fatEntry(c), FAT_EOC);
        return c;
    }

    /* Chains c2 after c1. */
    void linkCluster(uint32_t c1, uint32_t c2)
    {
        put32(fatEntry(c1), c2);
    }

    uint64_t clusterOffset(uint32_t c) const
    {
        return (uint64_t)(dataStart_ + (c - ROOT_CLUSTER) * spc_)
               * BYTES_PER_SECTOR;
    }

    uint32_t clusterSize() const { return spc_ * BYTES_PER_SECTOR; }

    /* Writes bytes into a cluster (intra-cluster offset). */
    void writeClusterData(uint32_t c, uint32_t inClusterOff,
                          const void* data, size_t len)
    {
        sink_.write(clusterOffset(c) + inClusterOff, data, len);
    }

    /* Writes both copies of the FAT and finalizes FSInfo. */
    void flush()
    {
        uint64_t fatOff = uint64_t(RESERVED_SECTORS) * BYTES_PER_SECTOR;
        sink_.write(fatOff, fat_.data(), fat_.size());
        sink_.write(fatOff + fat_.size(), fat_.data(), fat_.size());

        uint32_t freeClusters = clusterCount_ - usedClusters();
        uint32_t hint = nextFree_ < clusterCount_ + ROOT_CLUSTER
                        ? nextFree_ : 0xFFFFFFFFu;
        uint8_t info[512];
        for (uint16_t sec : {FSINFO_SECTOR, uint16_t(BACKUP_BOOT_SECTOR + 1)}) {
            std::memset(info, 0, sizeof(info));
            put32(info + 0x000, 0x41615252);     /* "RRaA"      */
            put32(info + 0x1E4, 0x61417272);     /* "rrAa"      */
            put32(info + 0x1E8, freeClusters);
            put32(info + 0x1EC, hint);
            info[0x1FE] = 0x55; info[0x1FF] = 0xAA;
            sink_.write(uint64_t(sec) * BYTES_PER_SECTOR, info, sizeof(info));
        }
    }

private:
    Sink& sink_;
    uint64_t volumeBytes_;
    std::string label_;
    std::vector<uint8_t> fat_;
    uint32_t spc_ = 1;              /* sectors per cluster           */
    uint32_t fatSectors_ = 0;
    uint32_t dataStart_ = 0;        /* first data sector             */
    uint32_t clusterCount_ = 0;
    uint32_t nextFree_ = ROOT_CLUSTER;
    uint32_t hiddenSectors_ = 0;    /* BPB_HiddSec (partition LBA)   */

    uint8_t* fatEntry(uint32_t c) { return fat_.data() + c * 4; }

    void computeGeometry(uint64_t volumeBytes)
    {
        uint64_t totalSectors = volumeBytes / BYTES_PER_SECTOR;
        if (totalSectors < 65536 + RESERVED_SECTORS)
            throw std::runtime_error("FAT32 volume too small (< 32 MiB)");

        /* Target cluster size based on the volume size (MS table),
         * adjusted to stay within the FAT32 cluster range. */
        uint64_t bytes = volumeBytes;
        uint32_t target;
        if      (bytes < 128ull * 1024 * 1024)          target = 1;
        else if (bytes < 256ull * 1024 * 1024)          target = 2;
        else if (bytes < 8ull * 1024 * 1024 * 1024)     target = 8;
        else                                             target = 64;

        uint32_t spc = target;
        for (;;) {
            /* STABLE fixed point: smallest fatSectors such that the FAT
             * can hold (clusters + 2) entries for the clusters
             * obtained with THIS fatSectors.  (The naive iteration
             * "newFat == fatSectors" oscillates on some sizes,
             * 64 MiB for example, and leaves an inconsistent geometry.) */
            uint32_t fatSectors = 1;
            uint64_t clusters = 0;
            for (int iter = 0; iter < 64; ++iter) {
                clusters = (totalSectors - RESERVED_SECTORS
                            - NUM_FATS * fatSectors) / spc;
                uint32_t need = uint32_t(((clusters + 2) * 4
                                          + BYTES_PER_SECTOR - 1)
                                         / BYTES_PER_SECTOR);
                if (need <= fatSectors)
                    break;      /* converged: capacity sufficient */
                fatSectors = need;
            }
            if (clusters >= MIN_CLUSTERS) {
                spc_ = spc;
                fatSectors_ = fatSectors;
                clusterCount_ = uint32_t(clusters);
                dataStart_ = RESERVED_SECTORS + NUM_FATS * fatSectors_;
                return;
            }
            if (spc == 1)
                throw std::runtime_error(
                    "FAT32 volume too small to hold 65525 clusters");
            spc /= 2;
        }
    }

    void writeBootSector(uint16_t at)
    {
        std::vector<uint8_t> b(BYTES_PER_SECTOR, 0);

        b[0] = 0xEB; b[1] = 0x58; b[2] = 0x90;              /* jump (0x5A) */
        std::memcpy(b.data() + 3, "PRISMOS ", 8);           /* OEM        */

        put16(b.data() + 0x0B, BYTES_PER_SECTOR);
        b[0x0D] = uint8_t(spc_);
        put16(b.data() + 0x0E, RESERVED_SECTORS);
        b[0x10] = NUM_FATS;
        put16(b.data() + 0x11, 0);                          /* RootEntCnt */
        put16(b.data() + 0x13, 0);                          /* TotSec16   */
        b[0x15] = 0xF8;                                     /* fixed media */
        put16(b.data() + 0x16, 0);                          /* FATSz16    */
        put16(b.data() + 0x18, 63);                         /* SecPerTrk  */
        put16(b.data() + 0x1A, 255);                        /* NumHeads   */
        put32(b.data() + 0x1C, hiddenSectors_);             /* HiddSec    */
        put32(b.data() + 0x20, uint32_t(volumeBytes_ / BYTES_PER_SECTOR));
        put32(b.data() + 0x24, fatSectors_);
        put16(b.data() + 0x28, 0);                          /* ExtFlags   */
        put16(b.data() + 0x2A, 0);                          /* FSVer      */
        put32(b.data() + 0x2C, ROOT_CLUSTER);
        put16(b.data() + 0x30, FSINFO_SECTOR);
        put16(b.data() + 0x34, BACKUP_BOOT_SECTOR);
        /* 0x36..0x3F: reserved (zeros)                          */

        b[0x40] = 0x80;                                     /* DrvNum     */
        b[0x41] = 0;
        b[0x42] = 0x29;                                     /* BootSig    */
        put32(b.data() + 0x43, 0x5052534D);                 /* VolID      */
        {
            char label[11] = {' ',' ',' ',' ',' ',' ',' ',' ',' ',' ',' '};
            for (size_t i = 0; i < 11 && i < label_.size(); ++i)
                label[i] = label_[i];
            std::memcpy(b.data() + 0x47, label, 11);
        }
        std::memcpy(b.data() + 0x52, "FAT32   ", 8);

        b[0x1FE] = 0x55; b[0x1FF] = 0xAA;
        sink_.write(uint64_t(at) * BYTES_PER_SECTOR, b.data(), b.size());
    }

    void writeFsInfo(uint16_t at)
    {
        /* Final content written in flush(); here, initial version. */
        std::vector<uint8_t> b(BYTES_PER_SECTOR, 0);
        put32(b.data() + 0x000, 0x41615252);
        put32(b.data() + 0x1E4, 0x61417272);
        b[0x1FE] = 0x55; b[0x1FF] = 0xAA;
        sink_.write(uint64_t(at) * BYTES_PER_SECTOR, b.data(), b.size());
    }
};

/* ------------------------------------------------------------------ */
/* Directory entries and short / long names.                            */
/* ------------------------------------------------------------------ */

struct ShortName {
    std::array<uint8_t, 11> sn{};      /* stored 8.3 form (spaces)      */
    bool needsLfn = false;             /* LFN required                   */
};

/* Checksum of an 8.3 name for LFN entries (Microsoft algorithm). */
uint8_t lfnChecksum(const uint8_t shortName[11])
{
    uint8_t sum = 0;
    for (int i = 0; i < 11; ++i)
        sum = uint8_t(((sum & 1) << 7) + (sum >> 1) + shortName[i]);
    return sum;
}

/* Characters accepted in an 8.3 name. */
bool validShortChar(unsigned char c)
{
    return (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
           c == '_' || c == '^' || c == '$' || c == '~' || c == '!' ||
           c == '#' || c == '%' || c == '&' || c == '-' || c == '{' ||
           c == '}' || c == '(' || c == ')' || c == '@' || c == '\'';
}

/* Generates the short form of a name; "needsLfn" tells whether the
 * exact name cannot be represented in 8.3. */
ShortName makeShortName(const std::string& name)
{
    std::string base, ext;
    auto dot = name.find_last_of('.');
    if (dot == std::string::npos || dot == 0) {
        base = name;
    } else {
        base = name.substr(0, dot);
        ext = name.substr(dot + 1);
    }

    auto sanitize = [](const std::string& s, size_t max, bool* lossy) {
        std::string out;
        if (s.size() > max) *lossy = true;
        for (unsigned char c : s) {
            if (validShortChar(c)) {
                out += char(std::toupper(c));
            } else {
                out += '_';
                *lossy = true;
            }
            if (out.size() == max) break;
        }
        return out;
    };

    bool lossy = false;
    std::string b = sanitize(base, 8, &lossy);
    std::string e = sanitize(ext, 3, &lossy);

    ShortName out;
    for (size_t i = 0; i < 8; ++i)
        out.sn[i] = i < b.size() ? uint8_t(b[i]) : ' ';
    for (size_t i = 0; i < 3; ++i)
        out.sn[8 + i] = i < e.size() ? uint8_t(e[i]) : ' ';

    /* Exact representation possible? (case included) */
    std::string reconstructed = b + (e.empty() ? "" : "." + e);
    out.needsLfn = lossy || reconstructed != name;
    return out;
}

/* Resolves an 8.3 collision in a directory by appending ~1, ~2... */
std::array<uint8_t, 11> uniquify(const std::array<uint8_t, 11>& sn,
                                 const std::set<std::array<uint8_t, 11>>& taken)
{
    if (!taken.count(sn)) return sn;

    std::string b(reinterpret_cast<const char*>(sn.data()), 8);
    std::string x(reinterpret_cast<const char*>(sn.data()) + 8, 3);
    b.erase(b.find_last_not_of(' ') + 1);
    x.erase(x.find_last_not_of(' ') + 1);

    for (int n = 1; n < 1000000; ++n) {
        std::string tail = "~" + std::to_string(n);
        std::string cand = b.substr(0, std::min(b.size(), 8 - tail.size()))
                           + tail;
        std::string full = cand + x;
        full.resize(11, ' ');
        std::array<uint8_t, 11> out{};
        std::memcpy(out.data(), full.data(), 11);
        if (!taken.count(out)) return out;
    }
    throw std::runtime_error("cannot make the 8.3 name unique");
}

/* 32-byte entries for a name: LFN (if needed) + short entry. */
std::vector<std::vector<uint8_t>> makeDirEntries(
    const std::string& name, const std::array<uint8_t, 11>& shortName,
    uint8_t attr, uint32_t firstCluster, uint32_t size, util::FatTime when)
{
    ShortName info = makeShortName(name);
    std::vector<std::vector<uint8_t>> out;

    if (info.needsLfn) {
        /* UTF-16LE name + terminator. */
        std::vector<uint16_t> u;
        for (char c : name) {
            if ((unsigned char)c > 0x7F)
                throw std::runtime_error(
                    "non-ASCII character in the ESP: " + name +
                    " (not handled by the installer)");
            u.push_back(uint16_t((unsigned char)c));
        }
        u.push_back(0x0000);

        size_t nEntries = (u.size() + 12) / 13;    /* 13 characters/entry  */
        uint8_t csum = lfnChecksum(shortName.data());

        for (size_t i = 0; i < nEntries; ++i) {
            std::vector<uint8_t> e(32, 0);
            size_t idx = nEntries - 1 - i;         /* from last to first      */
            e[0] = uint8_t((idx + 1) | (i == 0 ? 0x40 : 0));
            e[0x0B] = 0x0F;                        /* LFN attribute         */
            e[0x0C] = 0;
            e[0x0D] = csum;

            uint16_t chars13[13];
            for (int k = 0; k < 13; ++k) {
                size_t pos = idx * 13 + size_t(k);
                chars13[k] = pos < u.size() ? u[pos] : 0xFFFF;
            }
            for (int k = 0; k < 5; ++k) {
                e[1 + k * 2] = uint8_t(chars13[k]);
                e[2 + k * 2] = uint8_t(chars13[k] >> 8);
            }
            for (int k = 5; k < 11; ++k) {
                e[0x0E + (k - 5) * 2] = uint8_t(chars13[k]);
                e[0x0F + (k - 5) * 2] = uint8_t(chars13[k] >> 8);
            }
            for (int k = 11; k < 13; ++k) {
                e[0x1C + (k - 11) * 2] = uint8_t(chars13[k]);
                e[0x1D + (k - 11) * 2] = uint8_t(chars13[k] >> 8);
            }
            out.push_back(std::move(e));
        }
    }

    std::vector<uint8_t> e(32, 0);
    std::memcpy(e.data(), shortName.data(), 11);
    e[0x0B] = attr;
    e[0x0C] = 0;
    e[0x0D] = when.tenth > 99 ? 99 : uint8_t(when.tenth);
    e[0x0E] = uint8_t(when.time);
    e[0x0F] = uint8_t(when.time >> 8);
    e[0x10] = uint8_t(when.date);
    e[0x11] = uint8_t(when.date >> 8);
    e[0x12] = uint8_t(when.date);                 /* last access          */
    e[0x13] = uint8_t(when.date >> 8);
    e[0x14] = uint8_t(firstCluster >> 16);        /* FstClusHI            */
    e[0x15] = uint8_t(firstCluster >> 24);
    e[0x16] = uint8_t(when.time);                 /* write                */
    e[0x17] = uint8_t(when.time >> 8);
    e[0x18] = uint8_t(when.date);
    e[0x19] = uint8_t(when.date >> 8);
    e[0x1A] = uint8_t(firstCluster);
    e[0x1B] = uint8_t(firstCluster >> 8);
    e[0x1C] = uint8_t(size);
    e[0x1D] = uint8_t(size >> 8);
    e[0x1E] = uint8_t(size >> 16);
    e[0x1F] = uint8_t(size >> 24);
    out.push_back(std::move(e));
    return out;
}

std::vector<uint8_t> dotEntry(const std::array<uint8_t, 11>& name,
                              uint32_t firstCluster)
{
    std::vector<uint8_t> e(32, 0);
    std::memcpy(e.data(), name.data(), 11);
    e[0x0B] = 0x10;                                /* directory           */
    e[0x14] = uint8_t(firstCluster >> 16);
    e[0x15] = uint8_t(firstCluster >> 24);
    e[0x1A] = uint8_t(firstCluster);
    e[0x1B] = uint8_t(firstCluster >> 8);
    return e;
}

constexpr std::array<uint8_t, 11> DOT1 =
    {'.',' ',' ',' ',' ',' ',' ',' ',' ',' ',' '};
constexpr std::array<uint8_t, 11> DOT2 =
    {'.','.',' ',' ',' ',' ',' ',' ',' ',' ',' '};

/* ------------------------------------------------------------------ */
/* Recursive copy of the directory tree.                                */
/* ------------------------------------------------------------------ */

struct Copier {
    Volume* vol;
    std::function<void(const std::string&)> onFile;
    size_t files = 0, dirs = 0;
    uint64_t payload = 0;
    std::vector<uint8_t> chunk{};      /* single-cluster buffer          */

    /* Writes a list of entries into the chain starting at `first`,
     * allocating the needed clusters. */
    void writeDirData(uint32_t first,
                      const std::vector<std::vector<uint8_t>>& entries)
    {
        uint32_t cur = first;
        uint32_t off = 0;
        uint32_t cs = vol->clusterSize();

        for (const auto& e : entries) {
            if (off + e.size() > cs) {
                uint32_t next = vol->allocCluster();
                vol->linkCluster(cur, next);
                cur = next;
                off = 0;
            }
            vol->writeClusterData(cur, off, e.data(), e.size());
            off += uint32_t(e.size());
        }

        /* Tail of the last cluster explicitly zeroed (end of
         * directory = 0x00).  Real regression: only the entries
         * were written; on a reused medium, the end of the directory
         * kept old bytes immediately interpreted as additional
         * entries - the check failed with "unexpected
         * non-ASCII LFN" as soon as the attribute byte landed on
         * 0x0F.  Test images, implicitly zero, completely hid the
         * defect (FILE tails were already cleaned, cf. copyFile -
         * directories had been forgotten). */
        if (off < cs) {
            std::vector<uint8_t> zeros(size_t(cs - off), 0);
            vol->writeClusterData(cur, off, zeros.data(), zeros.size());
        }
    }

    /* Copies a file: returns (first cluster, size). */
    std::pair<uint32_t, uint32_t> copyFile(const fs::path& path,
                                           const std::string& relPath)
    {
        struct ::stat st{};
        if (::stat(path.string().c_str(), &st) != 0)
            throw std::runtime_error("stat failed: " + path.string());
        uint64_t size = uint64_t(st.st_size);
        if (size > 0xFFFFFFFFull)
            throw std::runtime_error("file too large for FAT32: "
                                     + path.string());

        FILE* f = std::fopen(path.string().c_str(), "rb");
        if (!f)
            throw std::runtime_error("open failed: " + path.string());

        uint32_t cs = vol->clusterSize();
        chunk.resize(cs);

        uint32_t first = 0;
        uint32_t prev = 0;
        uint64_t remaining = size;

        try {
            do {
                size_t want = size_t(std::min<uint64_t>(cs, remaining));
                if (want == 0) break;
                if (std::fread(chunk.data(), 1, want, f) != want)
                    throw std::runtime_error(
                        "incomplete read: " + path.string());

                uint32_t c = vol->allocCluster();
                if (first == 0) first = c;
                else vol->linkCluster(prev, c);
                prev = c;
                vol->writeClusterData(c, 0, chunk.data(), want);
                if (want < cs) {
                    /* partial cluster end: the rest is already zero
                     * on a blank disk; on reuse, it is cleaned
                     * explicitly. */
                    std::vector<uint8_t> zeros(cs - want, 0);
                    vol->writeClusterData(c, uint32_t(want), zeros.data(),
                                          zeros.size());
                }
                remaining -= want;
            } while (remaining > 0);
        } catch (...) {
            std::fclose(f);
            throw;
        }
        std::fclose(f);

        files += 1;
        payload += size;
        if (onFile) onFile(relPath);
        return {first, uint32_t(size)};
    }

    /* Copies a directory; returns the directory's first cluster. */
    uint32_t copyDir(const fs::path& src, uint32_t parentFirst,
                     const std::string& relPrefix,
                     const std::set<std::array<uint8_t, 11>>& taken)
    {
        /* Collect and deterministic sort of children (case-insensitive). */
        std::vector<fs::path> children;
        for (const auto& de : fs::directory_iterator(src))
            children.push_back(de.path());
        std::sort(children.begin(), children.end(),
                  [](const fs::path& a, const fs::path& b) {
                      auto wa = a.filename().wstring();
                      auto wb = b.filename().wstring();
                      return std::lexicographical_compare(
                          wa.begin(), wa.end(), wb.begin(), wb.end(),
                          [](wchar_t x, wchar_t y) {
                              return std::towlower(x) < std::towlower(y);
                          });
                  });

        uint32_t first = vol->allocCluster();

        std::vector<std::vector<uint8_t>> entries;
        if (relPrefix.empty()) {
            /* Root: volume label entry first (padded with
             * spaces, like mkfs.fat - zeros there would be read
             * as null characters by some tools). */
            std::vector<uint8_t> lab(32, 0);
            std::array<uint8_t, 11> sn;
            sn.fill(' ');
            for (size_t i = 0; i < 11 && i < vol->label().size(); ++i)
                sn[i] = uint8_t(vol->label()[i]);
            std::memcpy(lab.data(), sn.data(), 11);
            lab[0x0B] = 0x08;                     /* ATTR_VOLUME_ID */
            entries.push_back(std::move(lab));
        } else {
            entries.push_back(dotEntry(DOT1, first));
            entries.push_back(dotEntry(DOT2, parentFirst));
            dirs += 1;
        }

        /* Short names already used in THIS directory (anti-collision). */
        std::set<std::array<uint8_t, 11>> localTaken = taken;

        for (const auto& child : children) {
            std::string name = child.filename().string();
            if (name == "." || name == ".." || name.empty()) continue;
            if (name.find('/') != std::string::npos ||
                name.find('\\') != std::string::npos)
                throw std::runtime_error("invalid name in the ESP: " + name);

            std::string rel = relPrefix.empty()
                              ? name : relPrefix + "/" + name;

            ShortName info = makeShortName(name);
            info.sn = uniquify(info.sn, localTaken);
            localTaken.insert(info.sn);

            struct ::stat st{};
            ::stat(child.string().c_str(), &st);
            auto when = util::toFatTime(int64_t(st.st_mtime));

            if (fs::is_directory(child)) {
                /* The subdirectory's first cluster is reserved before
                 * its recursion; the parent entry is thus already complete. */
                uint32_t sub = copyDir(child, first, rel, {});
                for (auto& e : makeDirEntries(name, info.sn, 0x10, sub, 0, when))
                    entries.push_back(std::move(e));
            } else if (fs::is_regular_file(child)) {
                auto [fc, size] = copyFile(child, rel);
                for (auto& e : makeDirEntries(name, info.sn, 0x20, fc, size, when))
                    entries.push_back(std::move(e));
            } else {
                throw std::runtime_error(
                    "unhandled file type in the ESP: " + rel +
                    " (symbolic link?)");
            }
        }

        writeDirData(first, entries);
        return first;
    }
};

} // namespace

BuildResult buildVolume(int fd, uint64_t baseOffset, uint64_t volumeBytes,
                        const std::string& volumeLabel,
                        const std::string& srcDir,
                        const std::function<void(const std::string&)>& onFile)
{
    if (!fs::exists(srcDir) || !fs::is_directory(srcDir))
        throw std::runtime_error("ESP directory not found: " + srcDir);
    if (volumeLabel.size() > 11)
        throw std::runtime_error(
            "the FAT32 label must be at most 11 characters long");

    Sink sink(fd, baseOffset);
    Volume vol(sink, volumeBytes, volumeLabel);

    Copier copier{.vol = &vol, .onFile = onFile};
    copier.copyDir(srcDir, 0, "", {});
    vol.flush();

    BuildResult r;
    r.volumeBytes = volumeBytes;
    r.sectorsPerCluster = vol.sectorsPerCluster();
    r.clusterCount = vol.clusterCount();
    r.usedClusters = vol.usedClusters();
    r.files = copier.files;
    r.dirs = copier.dirs;
    r.payloadBytes = copier.payload;
    return r;
}

} // namespace prism::fat32
