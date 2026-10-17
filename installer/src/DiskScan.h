/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - system disk enumeration (sysfs) and safety
 * checks (mounted devices, in use).
 */

#pragma once

#include <cstdint>
#include <string>
#include <vector>

namespace prism {

struct DiskInfo {
    std::string dev;        /* /dev/sda, /dev/nvme0n1...           */
    std::string name;       /* sda, nvme0n1...                      */
    uint64_t sectors = 0;   /* size in 512-byte sectors            */
    uint64_t bytes() const { return sectors * 512; }
    std::string model;      /* "Samsung SSD 870"...                */
    std::string vendor;
    bool removable = false;
    bool readOnly  = false;

    /* Friendly label for the lists: /dev/sda - MODEL - 120 GiB */
    std::string describe() const;
};

/* Lists the physical disks (excludes ram/zram/dm/cdrom/loops).
 * Always returns a vector, possibly empty. */
std::vector<DiskInfo> scanDisks();

/* Returns the list of mount points using `dev` or one of its
 * partitions (e.g. /dev/sda3).  Empty if the disk is free. */
std::vector<std::string> mountsUsingDisk(const std::string& dev);

/* True if a partition of the disk is used as swap. */
bool diskHasSwap(const std::string& dev);

/* True if the execution is root (required to write to /dev/...). */
bool runningAsRoot();

/* True if `path` is a block device. */
bool isBlockDevice(const std::string& path);

} // namespace prism
