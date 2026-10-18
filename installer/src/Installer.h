/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - installation orchestrator.
 *
 * Steps:
 *   1. load/check the artifacts (boot0, boot1, loader, ESP tree,
 *      root tree),
 *   2. probe the target (block device or image) and safety,
 *   3. compute the partition plan (GPT: ESP, boot, ext4 root),
 *   4. write: protective MBR, GPT, ext4 root populated from the root
 *      tree (mke2fs -d), BIOS boot partition, FAT32 ESP (native
 *      writer, only the UEFI stub),
 *   5. a posteriori verification (re-read + CRC, ext4 superblock and
 *      kernel).
 *
 * The engine is synchronous and Qt-free: the GUI runs it in a thread,
 * the CLI directly.  Progress is reported through callbacks.
 */

#pragma once

#include "Fat32.h"
#include "Gpt.h"
#include "InstallOptions.h"

#include <functional>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

namespace prism {

/* User-oriented installation error. */
class InstallError : public std::runtime_error {
public:
    explicit InstallError(const std::string& msg)
        : std::runtime_error(msg) {}
};

/* Source artifacts loaded in memory / described. */
struct SourceInfo {
    std::string sourceDir;
    std::string espDir;               /* tree of P1: EFI/BOOT/BOOTX64.EFI */
    std::string rootDir;              /* tree of P3 (ext4 root)          */
    std::string kernelPath;           /* <rootDir>/boot/kernel/kernel    */
    std::vector<uint8_t> boot0;
    std::vector<uint8_t> boot1;
    std::vector<uint8_t> loader;

    size_t espFiles = 0;              /* files in the ESP tree           */
    uint64_t espTreeBytes = 0;        /* total data of the ESP tree      */
    size_t rootFiles = 0;             /* files in the root tree          */
    uint64_t rootTreeBytes = 0;       /* total data of the root tree     */
    uint64_t kernelBytes = 0;

    BootArtifacts artifacts() const
    {
        return BootArtifacts{boot0, boot1, loader};
    }
};

/* Probed target. */
struct TargetInfo {
    std::string path;
    bool isImage = false;             /* image file vs device            */
    uint64_t bytes = 0;
    std::string description;          /* disk model or file name         */
    bool removable = false;           /* sysfs "removable" bit (real
                                       * disks only); many firmwares
                                       * offer the EFI/BOOT/BOOTX64.EFI
                                       * path only to removable
                                       * media                            */
};

/* Installation summary. */
struct InstallSummary {
    PartitionPlan plan;
    fat32::BuildResult esp;
    uint64_t kernelBytes = 0;
    size_t rootFiles = 0;             /* files seeded into the ext4 root */
    bool dryRun = false;
};

/* Progress callbacks. */
struct ProgressHooks {
    /* Log: one line per important step. */
    std::function<void(const std::string&)> info;
    /* Verbose log: details (copied file, launched command...). */
    std::function<void(const std::string&)> detail;
    /* Overall progress: percent + step label. */
    std::function<void(int percent, const std::string& step)> progress;
};

class Installer {
public:
    explicit Installer(InstallOptions opts);

    /* ---- Isolated steps (GUI preview, CLI validation) ------------- */

    /* Loads and checks the artifacts from options.sourceDir. */
    SourceInfo loadSource() const;

    /* Probes the target (creates the image if options.imageSize and
     * the file is missing). */
    TargetInfo probeTarget() const;

    /* Returns blocking safety problems (mounted disk, swap,
     * insufficient privileges...).  Empty = safe to write. */
    std::vector<std::string> safetyProblems(const TargetInfo& target) const;

    /* Computes the partition plan; throws InstallError if the disk
     * is too small or the options inconsistent. */
    PartitionPlan computePlan(const TargetInfo& target,
                              const SourceInfo& source) const;

    /* ---- Full execution -------- */

    /* Installs everything.  Throws InstallError on failure.
     * Returns the summary. */
    InstallSummary run(const ProgressHooks& hooks) const;

    const InstallOptions& options() const { return opts_; }

private:
    InstallOptions opts_;
};

/* Name of partition node N of a disk (sda→sda3, nvme0n1→nvme0n1p3). */
std::string partitionNodeName(const std::string& diskDev, int n);

} // namespace prism
