/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - orchestrator implementation.
 */

#include "Installer.h"
#include "DiskScan.h"
#include "Ext4.h"
#include "Layout.h"
#include "Pe.h"
#include "Process.h"
#include "Util.h"
#include "Verifier.h"

#include <cctype>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <filesystem>
#include <sstream>

#include <linux/fs.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

namespace fs = std::filesystem;

namespace prism {

/* ---- Size parsing ----------------------------------------------------- */

uint64_t parseSize(const std::string& s)
{
    if (s.empty()) return 0;

    uint64_t multiplier = 1;
    std::string num = s;
    char suffix = char(std::tolower((unsigned char)s.back()));
    switch (suffix) {
        case 'k': multiplier = 1024ull;               num.pop_back(); break;
        case 'm': multiplier = 1024ull * 1024;        num.pop_back(); break;
        case 'g': multiplier = 1024ull * 1024 * 1024; num.pop_back(); break;
        case 't': multiplier = 1024ull * 1024 * 1024 * 1024; num.pop_back(); break;
        default: break;
    }
    if (num.empty() || num.find_first_not_of("0123456789") != std::string::npos)
        return 0;

    uint64_t value = 0;
    try {
        value = std::stoull(num);
    } catch (...) {
        return 0;
    }
    return value * multiplier;
}

/* ---- I/O helpers -------------------------------------------------------- */

namespace {

void pwriteAll(int fd, uint64_t off, const void* data, size_t len)
{
    const uint8_t* p = static_cast<const uint8_t*>(data);
    while (len > 0) {
        ssize_t n = ::pwrite(fd, p, len, off_t(off));
        if (n < 0) {
            if (errno == EINTR) continue;
            throw InstallError(std::string("unable to write (pwrite): ")
                               + std::strerror(errno));
        }
        off += uint64_t(n);
        p += n;
        len -= size_t(n);
    }
}

void writeZeros(int fd, uint64_t off, uint64_t len)
{
    std::vector<uint8_t> zeros(1024 * 1024, 0);
    while (len > 0) {
        size_t take = size_t(std::min<uint64_t>(zeros.size(), len));
        pwriteAll(fd, off, zeros.data(), take);
        off += take;
        len -= take;
    }
}

struct FdGuard {
    int fd;
    explicit FdGuard(int f) : fd(f) {}
    ~FdGuard() { if (fd >= 0) ::close(fd); }
};

/* First line of process output, truncated (logs). */
std::string firstLine(std::string s, size_t maxLen = 160)
{
    auto nl = s.find('\n');
    if (nl != std::string::npos) s.resize(nl);
    if (s.size() > maxLen) s.resize(maxLen);
    return s;
}

} // namespace

/* ---- UEFI boot entry (efibootmgr, --efi-boot-entry option) ---------- */

namespace {

void createEfiBootEntry(const TargetInfo& target, const PartitionPlan& plan,
                        const std::string& efibootmgrPath,
                        const std::function<void(const std::string&)>& info)
{
    /* The default path \EFI\BOOT\BOOTX64.EFI is tried by firmwares
     * for REMOVABLE MEDIA; however, some firmwares treat a USB drive
     * as a fixed disk and only try the entries of the boot menu
     * (NVRAM).  This step, copied from the installers of the major
     * distributions, covers that case: without it, a perfectly
     * installed USB drive can appear "non-bootable" on such
     * machines. */
    (void)plan;

    if (!fs::exists("/sys/firmware/efi")
        && !::getenv("PRISM_INSTALLER_FORCE_EFIBOOTMGR")) {
        info("UEFI boot entry not created: this machine is not"
             " booted in UEFI (/sys/firmware/efi missing)");
        return;
    }

    std::string program = !efibootmgrPath.empty() ? efibootmgrPath
                                                  : prism::which("efibootmgr");
    if (program.empty()) {
        info("warning: efibootmgr not found - boot entry \"PrismOS\""
             " not created (install the efibootmgr package if the firmware"
             " ignores removable USB drives)");
        return;
    }

    /* No duplicate: if a "PrismOS" entry already exists, keep the
     * current one (efibootmgr creates a new entry on every call). */
    try {
        auto existing = runProcess(program, {}, {}, 15);
        if (existing.ok()
            && existing.stdoutText.find("PrismOS") != std::string::npos) {
            info("boot entry \"PrismOS\" already present in the"
                 " firmware menu: nothing to create");
            return;
        }
    } catch (const std::exception&) {
        /* Listing failed: creation will still be attempted. */
    }

    /* The ESP is the 1st GPT partition (entry #0 of the entry array). */
    std::vector<std::string> args{
        "--create", "--disk", target.path,
        "--part", "1",
        "--label", "PrismOS",
        "--loader", "\\EFI\\BOOT\\BOOTX64.EFI",
    };
    try {
        auto res = runProcess(program, args, {}, 15);
        if (res.ok())
            info("boot entry \"PrismOS\" created in the firmware"
                 " menu (efibootmgr)");
        else
            info("warning: efibootmgr failed (code "
                 + std::to_string(res.exitCode) + "): "
                 + firstLine(res.stderrText.empty() ? res.stdoutText
                                                    : res.stderrText));
    } catch (const std::exception& ex) {
        info(std::string("warning: efibootmgr unavailable: ")
             + ex.what());
    }
}

} // namespace

/* ---- Construction -------------------------------------------------------- */

Installer::Installer(InstallOptions opts) : opts_(std::move(opts)) {}

/* ---- Source --------------------------------------------------------------- */

SourceInfo Installer::loadSource() const
{
    SourceInfo src;

    if (opts_.sourceDir.empty())
        throw InstallError("source directory not set (PrismOS build"
                           " output, containing the expected build/...: "
                           "boot0.bin, boot1.bin, loader.bin, esp/)");
    if (!fs::exists(opts_.sourceDir) || !fs::is_directory(opts_.sourceDir))
        throw InstallError("source directory not found: "
                           + opts_.sourceDir);

    fs::path root(opts_.sourceDir);
    src.sourceDir = opts_.sourceDir;
    src.espDir = opts_.espDir.empty()
                 ? (root / "esp").string()
                 : opts_.espDir;

    auto missing = [&](const fs::path& p, const char* what) {
        if (!fs::exists(p))
            throw InstallError(std::string(what) + " not found: "
                               + p.string()
                               + " - run \"make\" at the PrismOS root");
    };

    fs::path boot0 = root / "boot0.bin";
    fs::path boot1 = root / "boot1.bin";
    fs::path loader = root / "loader.bin";
    missing(boot0, "boot0.bin");
    missing(boot1, "boot1.bin");
    missing(loader, "loader.bin");
    missing(fs::path(src.espDir), "the ESP tree (esp/)");

    src.kernelPath = (fs::path(src.espDir) / "boot" / "kernel" / "kernel").string();
    if (!fs::exists(src.kernelPath))
        throw InstallError("kernel missing from the ESP: " + src.kernelPath
                           + " - the installer requires a kernel to install");

    /* UEFI boot: without EFI/BOOT/BOOTX64.EFI (a valid x86-64 EFI
     * application), the disk boots in BIOS but NOT in UEFI - better
     * to refuse BEFORE erasing the disk than to install a non-bootable
     * USB drive.  A stale tree (incomplete build, manual copy, foreign
     * architecture) is exactly the kind of surprise this check
     * catches. */
    fs::path bootx64 = fs::path(src.espDir) / "EFI" / "BOOT" / "BOOTX64.EFI";
    if (!fs::exists(bootx64))
        throw InstallError(
            "UEFI boot missing from the ESP: EFI/BOOT/BOOTX64.EFI (in "
            + src.espDir + ") - the UEFI firmware would have nothing"
            " to boot.  Rebuild at the PrismOS root: make"
            " (the esp/ tree is produced there by \"make esp\"");
    {
        std::vector<uint8_t> efiApp;
        try {
            efiApp = util::readFile(bootx64.string());
        } catch (const std::exception& ex) {
            throw InstallError(std::string("unable to read the UEFI "
                                           "boot file: ") + ex.what());
        }
        std::string peErr = pe::validateEfiAppBytes(efiApp.data(),
                                                    efiApp.size());
        if (!peErr.empty())
            throw InstallError("EFI/BOOT/BOOTX64.EFI is not a"
                               " valid x86-64 UEFI application: " + peErr
                               + " - rebuild at the PrismOS root"
                               " (make)");
    }

    try {
        src.boot0  = util::readFile(boot0.string());
        src.boot1  = util::readFile(boot1.string());
        src.loader = util::readFile(loader.string());
    } catch (const std::exception& ex) {
        throw InstallError(std::string("unable to read artifacts: ")
                           + ex.what());
    }

    std::string v = src.artifacts().validate();
    if (!v.empty())
        throw InstallError("invalid boot artifacts: " + v);

    /* Inventory of the ESP tree. */
    std::error_code ec;
    for (const auto& de : fs::recursive_directory_iterator(src.espDir, ec)) {
        if (ec) break;
        if (de.is_regular_file(ec)) {
            src.espFiles += 1;
            src.espTreeBytes += uint64_t(de.file_size(ec));
        }
    }
    if (src.espFiles == 0)
        throw InstallError("the ESP tree is empty: "
                           + src.espDir);
    src.kernelBytes = uint64_t(fs::file_size(src.kernelPath, ec));

    return src;
}

/* ---- Target ----------------------------------------------------------------- */

TargetInfo Installer::probeTarget() const
{
    TargetInfo t;
    t.path = opts_.target;
    if (t.path.empty())
        throw InstallError("target not set (disk /dev/... or image)");

    if (util::isBlockDevice(t.path)) {
        t.isImage = false;
        int fd = ::open(t.path.c_str(), O_RDONLY);
        if (fd < 0)
            throw InstallError(std::string("unable to open: ")
                               + t.path + ": " + std::strerror(errno));
        FdGuard g(fd);
        uint64_t bytes = 0;
        if (ioctl(fd, BLKGETSIZE64, &bytes) != 0)
            throw InstallError(std::string("disk size unreadable: ")
                               + t.path + ": " + std::strerror(errno));
        t.bytes = bytes;

        std::string base = fs::path(t.path).filename().string();
        for (const auto& d : scanDisks()) {
            if (d.name == base) {
                t.description = d.describe();
                t.removable = d.removable;
                break;
            }
        }
        if (t.description.empty())
            t.description = t.path + " - " + humanSize(bytes);
        return t;
    }

    if (fs::exists(t.path)) {
        if (!util::isRegularFile(t.path))
            throw InstallError("the target is neither a block device nor "
                               "an image file: " + t.path);
        t.isImage = true;
        std::error_code ec;
        t.bytes = uint64_t(fs::file_size(t.path, ec));
        t.description = fs::path(t.path).filename().string()
                        + " (image) - " + humanSize(t.bytes);
        if (t.bytes == 0)
            throw InstallError("empty image file: " + t.path);
        return t;
    }

    /* Creation of a fresh image when a size is provided.
     * In simulation mode (--dry-run), the file is NOT created: we
     * simply describe the virtual target. */
    if (opts_.imageSize > 0) {
        t.isImage = true;
        t.bytes = opts_.imageSize;
        t.description = fs::path(t.path).filename().string()
                        + " (new image"
                        + std::string(opts_.dryRun ? ", simulated" : "")
                        + ") - " + humanSize(t.bytes);
        if (opts_.dryRun)
            return t;

        int fd = ::open(t.path.c_str(), O_RDWR | O_CREAT | O_EXCL, 0644);
        if (fd < 0)
            throw InstallError(std::string("unable to create: ")
                               + t.path + ": " + std::strerror(errno));
        if (ftruncate(fd, off_t(opts_.imageSize)) != 0) {
            ::close(fd);
            throw InstallError(std::string("unable to set the size: ")
                               + t.path + ": " + std::strerror(errno));
        }
        ::close(fd);
        return t;
    }

    throw InstallError("target not found: " + t.path
                       + " (specify --image-size to create it)");
}

/* ---- Safety -------------------------------------------------------------------- */

std::vector<std::string> Installer::safetyProblems(const TargetInfo& t) const
{
    std::vector<std::string> problems;

    if (t.isImage) {
        /* An image cannot be the target of a regular mount;
         * nothing to block a priori. */
        return problems;
    }

    if (!runningAsRoot())
        problems.push_back("writing a disk requires administrator "
                           "privileges (normally requested "
                           "automatically at launch - see "
                           "PRISM_INSTALLER_NO_ELEVATE)");

    auto mounts = mountsUsingDisk(t.path);
    if (!mounts.empty()) {
        std::string list;
        for (size_t i = 0; i < mounts.size() && i < 5; ++i)
            list += (i ? ", " : "") + mounts[i];
        problems.push_back("partitions of " + t.path
                           + " are mounted (" + list + "): unmount them "
                           "before installing");
    }

    if (diskHasSwap(t.path))
        problems.push_back("a partition of " + t.path
                           + " is used as swap: disable it (swapoff)");

    std::string base = fs::path(t.path).filename().string();
    if (!fs::exists("/sys/block/" + base))
        problems.push_back(t.path + " appears to be a partition, not a "
                           "whole disk; target the disk (e.g. /dev/sda)");

    return problems;
}

/* ---- Plan ------------------------------------------------------------------------- */

PartitionPlan Installer::computePlan(const TargetInfo& t,
                                     const SourceInfo& src) const
{
    if (opts_.espMiB < ESP_MIN_MIB || opts_.espMiB > ESP_MAX_MIB)
        throw InstallError("invalid ESP size ("
                           + std::to_string(opts_.espMiB) + " MiB; "
                           "between " + std::to_string(ESP_MIN_MIB) + " and "
                           + std::to_string(ESP_MAX_MIB) + ")");
    if (opts_.reserveMiB > RESERVE_MAX_MIB)
        throw InstallError("reserve too large (max "
                           + std::to_string(RESERVE_MAX_MIB) + " MiB)");
    if (opts_.ext4MiB != 0 && opts_.ext4MiB < EXT4_MIN_MIB)
        throw InstallError("ext4 partition too small (min "
                           + std::to_string(EXT4_MIN_MIB) + " MiB)");
    if (opts_.espLabel.size() > 11)
        throw InstallError("the ESP label must be at most 11 characters");
    if (opts_.ext4Label.size() > 16)
        throw InstallError("the ext4 label must be at most 16 characters");

    /* The ESP must hold the tree + FAT metadata (FAT + entries),
     * with a comfortable margin. */
    uint64_t fatOverhead = src.espTreeBytes / 16;    /* ~FAT*2 at 512 B/cluster */
    uint64_t needed = src.espTreeBytes + fatOverhead + 2 * MIB;
    if (opts_.espMiB * MIB < needed)
        throw InstallError("ESP too small: the tree is "
                           + humanSize(src.espTreeBytes)
                           + ", allow at least "
                           + humanSize(needed + MIB) + " (--esp-size option)");

    PartitionPlan p;
    p.totalSectors = t.bytes / SECTOR;
    if (p.totalSectors < 2048 + 4096 + 2048 + 33 + 2048)
        throw InstallError("disk too small (" + humanSize(t.bytes) + ")");

    p.lastLba     = p.totalSectors - 1;
    p.firstUsable = 34;
    p.lastUsable  = p.lastLba - 33;

    /* Layout: the ESP is the FIRST partition (P1, LBA 2048) because
     * some OEM firmwares only try the default boot path
     * (EFI/BOOT/BOOTX64.EFI) on the first partition of a removable
     * medium - with the BIOS boot partition first, the USB drive
     * could be installed perfectly and still never boot on such
     * machines.  Then: unallocated reserve (growth margin for the
     * ESP), BIOS boot partition (P2, LBA patched into boot0/boot1 at
     * install time), ext4 (P3). */
    p.espStart    = ESP_START;
    p.espSectors  = mibToSectors(opts_.espMiB);
    p.reserveSectors = mibToSectors(opts_.reserveMiB);
    p.bootStart   = p.espStart + p.espSectors + p.reserveSectors;
    p.bootSectors = BOOT_PART_SECTORS;
    p.ext4Start   = p.bootStart + p.bootSectors;

    if (p.ext4Start > p.lastUsable)
        throw InstallError("disk too small (" + humanSize(t.bytes)
                           + "): after the ESP of "
                           + std::to_string(opts_.espMiB) + " MiB, the "
                           "reserve of " + std::to_string(opts_.reserveMiB)
                           + " MiB and the boot partition, no space"
                           " remains for the ext4 partition");

    uint64_t ext4Avail = p.lastUsable - p.ext4Start + 1;
    uint64_t ext4Sectors = opts_.ext4MiB ? mibToSectors(opts_.ext4MiB)
                                         : ext4Avail;
    if (ext4Sectors > ext4Avail)
        throw InstallError("disk too small: "
                           + humanSize((ext4Sectors - ext4Avail) * SECTOR)
                           + " would be missing for the requested ext4 partition");
    if (ext4Sectors < mibToSectors(EXT4_MIN_MIB))
        throw InstallError("insufficient space for the ext4 partition ("
                           + humanSize(ext4Avail * SECTOR) + " remaining, min "
                           + std::to_string(EXT4_MIN_MIB) + " MiB)");
    p.ext4Sectors = ext4Sectors;

    p.diskGuid = util::Uuid::randomV4();
    p.bootGuid = util::Uuid::randomV4();
    p.espGuid  = util::Uuid::randomV4();
    p.ext4Guid = util::Uuid::randomV4();

    p.espLabel  = opts_.espLabel;
    p.ext4Label = opts_.ext4Label;
    return p;
}

/* ---- Execution --------------------------------------------------------------------- */

std::string partitionNodeName(const std::string& diskDev, int n)
{
    std::string base = fs::path(diskDev).filename().string();
    std::string suffix = std::to_string(n);
    if (!base.empty() && std::isdigit((unsigned char)base.back()))
        return diskDev + "p" + suffix;
    return diskDev + suffix;
}

InstallSummary Installer::run(const ProgressHooks& hooks) const
{
    auto info = [&](const std::string& m) {
        if (hooks.info) hooks.info(m);
    };
    auto detail = [&](const std::string& m) {
        if (hooks.detail) hooks.detail(m);
    };
    auto prog = [&](int percent, const std::string& step) {
        if (hooks.progress) hooks.progress(percent, step);
    };

    /* 1. Validation -------------------------------------------------- */
    prog(0, "Validating sources");
    SourceInfo src = loadSource();
    info("Artifacts: boot0 (512 B), boot1 (8 KiB), loader "
         + humanSize(src.loader.size())
         + ", kernel " + humanSize(src.kernelBytes));
    info("ESP tree: " + std::to_string(src.espFiles) + " files, "
         + humanSize(src.espTreeBytes));

    prog(2, "Validating the target");
    TargetInfo target = probeTarget();
    info("Target: " + target.description);

    auto problems = safetyProblems(target);
    if (!problems.empty()) {
        std::string msg = "installation refused:";
        for (const auto& p : problems) msg += "\n  - " + p;
        throw InstallError(msg);
    }

    prog(4, "Computing the partition plan");
    PartitionPlan plan = computePlan(target, src);

    info("Partition plan (GPT):");
    info("  Protective MBR + GPT headers (33 sectors at the end of the disk)");
    info("  P1 \"" + std::string(PART_NAME_ESP) + "\"  LBA "
         + std::to_string(plan.espStart) + " ("
         + humanSize(plan.espBytes()) + ") - FAT32, EFI + scripts + kernel"
         " - FIRST partition (required by some firmwares)");
    info("  - unallocated reserved space: "
         + humanSize(plan.reserveBytes())
         + " (margin for kernel growth; the ESP can be expanded there)");
    info("  P2 \"" + std::string(PART_NAME_BOOT) + "\"  LBA "
         + std::to_string(plan.bootStart) + " ("
         + humanSize(plan.bootSectors * SECTOR) + ") - boot1 + BIOS loader");
    info("  P3 \"" + std::string(PART_NAME_DATA) + "\"  LBA "
         + std::to_string(plan.ext4Start) + " ("
         + humanSize(plan.ext4Bytes()) + ") - ext4 \""
         + plan.ext4Label + "\"");

    InstallSummary summary;
    summary.plan = plan;
    summary.kernelBytes = src.kernelBytes;

    if (opts_.dryRun) {
        info("[SIMULATION] no writes performed.");
        summary.dryRun = true;
        prog(100, "Simulation complete");
        return summary;
    }

    /* 2. Opening ------------------------------------------------------ */
    int fd = ::open(target.path.c_str(), O_RDWR);
    if (fd < 0)
        throw InstallError(std::string("unable to open the target: ")
                           + target.path + ": " + std::strerror(errno));
    FdGuard guard(fd);

    try {
        /* 3. Cleaning sensitive areas -------------------------------- */
        prog(6, "Cleanup");
        info("Erasing existing signatures...");
        writeZeros(fd, 0, MIB);                              /* MBR + GPT */
        if (plan.reserveSectors)
            writeZeros(fd, plan.espStart * SECTOR + plan.espBytes(),
                       plan.reserveBytes());                 /* reserve    */
        writeZeros(fd, (plan.lastUsable + 1) * SECTOR,
                   uint64_t(plan.totalSectors - plan.lastUsable - 1) * SECTOR);

        /* 4. MBR + GPT ----------------------------------------------- */
        prog(9, "Writing the partition table");
        BootArtifacts art = src.artifacts();

        auto mbr = buildProtectiveMbr(art, plan);
        pwriteAll(fd, 0, mbr.data(), mbr.size());
        detail("Protective MBR written (boot0 + 0xEE entry)");

        auto entries = buildGptEntries(plan);
        uint32_t entriesCrc = util::crc32(entries);
        auto primary = buildGptHeader(plan, entriesCrc, false);
        auto backup  = buildGptHeader(plan, entriesCrc, true);
        pwriteAll(fd, SECTOR, primary.data(), primary.size());
        pwriteAll(fd, 2 * SECTOR, entries.data(), entries.size());
        pwriteAll(fd, (plan.lastLba - 32) * SECTOR, entries.data(),
                  entries.size());
        pwriteAll(fd, plan.lastLba * SECTOR, backup.data(), backup.size());
        detail("GPT written (primary + backup, 3 partitions)");

        /* 5. Partition ext4 ------------------------------------------- */
        /* Deliberately BEFORE writing the boot code and the ESP:
         * formatting (mke2fs - including discard/UNMAP) and, on a real
         * disk, the kernel's re-read of the partition table
         * (BLKRRPART, which invalidates the disk's page cache) are
         * thus finished before the boot and ESP writes.  Afterwards
         * NOTHING comes between those writes and their verification
         * re-read: some USB media lose sectors written shortly before
         * an UNMAP, and the previous order (ESP then mke2fs) made
         * verification random. */
        prog(12, "Formatting the ext4 partition");
        if (target.isImage) {
            ext4::FormatRequest req;
            req.target = target.path;
            req.offsetBytes = plan.ext4Start * SECTOR;
            req.sizeBytes = plan.ext4Bytes();
            req.label = plan.ext4Label;
            req.mke2fsPath = opts_.mke2fsPath;
            detail("mke2fs -E offset=" + std::to_string(req.offsetBytes)
                   + " (image file)");
            ext4::format(req);
        } else {
            if (ioctl(fd, BLKRRPART) != 0) {
                info("warning: unable to re-read the partition table "
                     "(" + std::string(std::strerror(errno))
                     + "); continuing...");
            }
            std::string node = partitionNodeName(target.path, 3);
            bool found = false;
            for (int i = 0; i < 50 && !found; ++i) {
                if (::access(node.c_str(), F_OK) == 0) found = true;
                else usleep(100000);
            }
            if (!found)
                throw InstallError("the partition node " + node
                                   + " does not appear; retry or "
                                   "reconnect the disk");
            ext4::FormatRequest req;
            req.target = node;
            req.label = plan.ext4Label;
            req.mke2fsPath = opts_.mke2fsPath;
            detail("mke2fs " + node);
            ext4::format(req);
        }
        info("ext4 partition \"" + plan.ext4Label + "\" formatted ("
             + humanSize(plan.ext4Bytes()) + ")");

        /* 6. BIOS boot partition -------------------------------------- */
        prog(15, "Installing the bootloader");
        auto bootPart = buildBootPartition(art, plan);
        pwriteAll(fd, plan.bootStart * SECTOR, bootPart.data(),
                  bootPart.size());
        info("boot1 (patched LBA " + std::to_string(plan.bootStart + BOOT1_SECTORS)
             + ") + loader installed in the boot partition");

        /* 7. ESP FAT32 ------------------------------------------------ */
        prog(17, "Installing the ESP (FAT32)");
        size_t filesDone = 0;
        size_t totalFiles = std::max<size_t>(src.espFiles, 1);
        auto onFile = [&](const std::string& p) {
            filesDone += 1;
            detail("  + " + p);
            int pct = 17 + int(53.0 * double(filesDone)
                               / double(totalFiles));
            prog(std::min(pct, 70), "Installing the ESP (FAT32)");
        };
        auto espRes = fat32::buildVolume(fd, plan.espStart * SECTOR,
                                         plan.espBytes(), opts_.espLabel,
                                         src.espDir, onFile);
        summary.esp = espRes;
        info("ESP FAT32 ready: " + std::to_string(espRes.files)
             + " files (" + humanSize(espRes.payloadBytes) + "), clusters of "
             + std::to_string(espRes.sectorsPerCluster * 512) + " B");

        ::fsync(fd);

        /* 8. Verification --------------------------------------------- */
        /* "Physical" re-read: fsync first, then drop the clean cache
         * pages (POSIX_FADV_DONTNEED) so that verification actually
         * re-reads the media and not memory.  An error here then
         * signals a real problem (failing media, dubious cable/port)
         * instead of hiding it. */
        prog(90, "Verifying the installation");
        ::posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);

        auto gptProblems = verify::verifyGptBoot(fd, mbr, entries,
                                                 plan.totalSectors,
                                                 plan.bootStart, bootPart);
        for (const auto& p : gptProblems)
            problems.push_back(p);

        auto espProblems = verify::verifyEsp(fd, plan.espStart * SECTOR,
                                             src.espDir, nullptr);
        for (const auto& p : espProblems)
            problems.push_back(p);

        /* UEFI chain: the firmware must find EFI/BOOT/BOOTX64.EFI
         * (re-read from the media) and be able to load it (PE/COFF). */
        auto uefiProblems = verify::verifyUefiBoot(fd, plan.espStart * SECTOR);
        for (const auto& p : uefiProblems)
            problems.push_back(p);

        if (!problems.empty()) {
            std::string msg = "verification failed:";
            for (const auto& p : problems) msg += "\n  - " + p;
            if (!espProblems.empty())
                msg += "\n  (bytes re-read from the media itself: if "
                       "the error persists after another attempt, "
                       "suspect the USB drive or its port - try "
                       "another port/cable)";
            throw InstallError(msg);
        }
        info("Verification: GPT, boot and ESP re-read and correct ("
             + std::to_string(src.espFiles) + " files checked by CRC)");
        info("UEFI boot: EFI/BOOT/BOOTX64.EFI re-read from the media -"
             " valid x86-64 EFI application");

        ::fsync(fd);
        prog(100, "Done");
    } catch (...) {
        ::fsync(fd);
        throw;
    }

    /* Optional UEFI boot entry (efibootmgr): some firmwares only try
     * the default path for genuinely removable media - see
     * createEfiBootEntry above. */
    if (opts_.efiBootEntry
        && (!target.isImage
            || ::getenv("PRISM_INSTALLER_FORCE_EFIBOOTMGR")))
        createEfiBootEntry(target, plan, opts_.efibootmgrPath, info);

    info("");
    info("Installation complete on " + target.path + ".");
    if (!target.isImage)
        info("The disk boots in BIOS and UEFI; the ext4 partition will "
             "be mounted on / by the PrismOS kernel.");

    /* Secure Boot: the PrismOS boot code is not signed.  Verified on
     * a real EDK2 firmware (OVMF): an otherwise PERFECTLY bootable
     * image is rejected ("Access Denied" then "No bootable option")
     * as soon as Secure Boot is active - the USB drive then appears
     * "non bootable" with no other clue.  Warn here, before the
     * user reboots and wastes an hour searching. */
    if (!target.isImage && util::secureBootEnabled() == 1) {
        info("");
        info("warning: Secure Boot is ENABLED on this machine -"
             " the firmware will refuse the unsigned PrismOS boot and"
             " the USB drive will seem non-bootable.");
        info("Disable Secure Boot in the firmware setup (F2/Esc"
             " at startup, \"Security\" menu) before booting PrismOS.");
    }

    /* Boot entry selection: the firmware menu (F12 on most
     * manufacturers' machines) often shows TWO entries for the same
     * USB drive - "UEFI: ..." and "USB HDD: ..." (legacy).  Only the
     * first uses EFI/BOOT/BOOTX64.EFI; the second launches the BIOS
     * chain.  And if only the legacy entry appears, the setup boot
     * mode is set to "Legacy/CSM": the firmware will never look for
     * the UEFI boot.  A reminder costs less than an hour of
     * diagnostics. */
    if (!target.isImage) {
        info("");
        info("At reboot: boot menu (often F12) → choose the UEFI entry"
             " for the USB drive (\"UEFI: ...\", often named after the"
             " volume: \"UEFI: PRISMOS\"), not \"USB HDD: ...\".");
        info("If only a \"USB HDD\" entry appears, switch the setup Boot"
             " Mode to UEFI (and disable CSM/Launch CSM).");

        /* Some firmwares (Insyde/AMI) only list in the menu GPT
         * disks with a partition carrying the "legacy BIOS bootable"
         * attribute - set since this version - or only try the
         * default path for removable media. */
        if (!target.removable) {
            info("");
            info("note: this device reports itself as a FIXED disk"
                 " (non-removable) - many firmwares will then not"
                 " automatically offer EFI/BOOT/BOOTX64.EFI.");
            info("Create an explicit boot entry with the"
                 " --efi-boot-entry option, or choose the \"PrismOS\""
                 " entry in the menu if it already exists.");
        }

        /* "The PC does not find the USB drive" even though it boots
         * under QEMU/OVMF: the media is fine, what remains is the
         * firmware's USB enumeration.  The three reflexes that solve
         * nearly all observed cases. */
        info("");
        info("If the USB drive does not appear AT ALL in the menu: perform"
             " a full shutdown of the machine (not a simple restart),"
             " reconnect the drive to another port - preferably a USB2"
             " port - then power back on and open the menu without waiting.");
        info("Also check in the setup (F2): \"USB Boot\" enabled and"
             " \"Fast Boot\" disabled (fast boot skips"
             " USB initialization).");
    }
    return summary;
}

} // namespace prism
