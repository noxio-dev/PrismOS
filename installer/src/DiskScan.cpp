/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - disk scan implementation.
 */

#include "DiskScan.h"
#include "Layout.h"
#include "Util.h"

#include <algorithm>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <sstream>

#include <sys/stat.h>
#include <unistd.h>

namespace prism {

namespace {

/* Reads a whole sysfs file, dropping the trailing newline. */
std::string sysRead(const std::string& path)
{
    std::ifstream f(path);
    if (!f) return "";
    std::string s;
    std::getline(f, s);
    return s;
}

bool startsWith(const std::string& s, const char* p)
{
    return s.rfind(p, 0) == 0;
}

} // namespace

std::vector<DiskInfo> scanDisks()
{
    std::vector<DiskInfo> out;

    for (const auto& e : std::filesystem::directory_iterator("/sys/block")) {
        std::string name = e.path().filename().string();

        /* Only real disks are offered. */
        if (startsWith(name, "ram") || startsWith(name, "zram") ||
            startsWith(name, "dm-")  || startsWith(name, "sr")   ||
            startsWith(name, "loop") || startsWith(name, "md"))
            continue;

        DiskInfo d;
        d.name = name;
        d.dev = "/dev/" + name;

        /* nvme/mmcblk partitions are named <base>p<N>: the disk
         * itself is indeed /dev/nvme0n1. */
        if (!std::filesystem::exists(d.dev))
            continue;

        uint64_t sectors = 0;
        try {
            sectors = std::stoull(sysRead(e.path().string() + "/size"));
        } catch (...) {
            continue;
        }
        if (sectors == 0) continue;
        d.sectors = sectors;

        d.model    = sysRead(e.path().string() + "/device/model");
        d.vendor   = sysRead(e.path().string() + "/device/vendor");
        d.removable = sysRead(e.path().string() + "/removable") == "1";
        d.readOnly  = sysRead(e.path().string() + "/ro") == "1";

        out.push_back(std::move(d));
    }

    std::sort(out.begin(), out.end(),
              [](const DiskInfo& a, const DiskInfo& b) {
                  return a.bytes() > b.bytes();
              });
    return out;
}

std::string DiskInfo::describe() const
{
    std::string label = model;
    if (!vendor.empty() && !label.empty())
        label = vendor + " " + label;
    if (label.empty())
        label = "disk";

    /* Cleanup of the repeated spaces coming from sysfs. */
    std::string clean;
    bool lastSpace = true;
    for (char c : label) {
        if (c == ' ' || c == '\t') {
            if (!lastSpace) { clean += ' '; lastSpace = true; }
        } else {
            clean += c;
            lastSpace = false;
        }
    }

    std::ostringstream os;
    os << dev << " - " << clean << " - " << humanSize(bytes());
    if (removable) os << " [removable]";
    if (readOnly)  os << " [read-only]";
    return os.str();
}

std::vector<std::string> mountsUsingDisk(const std::string& dev)
{
    std::vector<std::string> mounts;
    std::ifstream f("/proc/self/mountinfo");
    std::string line;
    while (std::getline(f, line)) {
        /* Fields: id parent major:minor root mount point options... */
        std::istringstream is(line);
        std::string id, parent, majmin, root, mountPoint;
        is >> id >> parent >> majmin >> root >> mountPoint;

        /* Octal decoding of the escapes (\040 for a space...). */
        std::string decoded;
        for (size_t i = 0; i < mountPoint.size(); ++i) {
            if (mountPoint[i] == '\\' && i + 2 < mountPoint.size() + 1 &&
                isxdigit((unsigned char)mountPoint[i + 1]) &&
                isxdigit((unsigned char)mountPoint[i + 2])) {
                auto hexv = [](char c) -> int {
                    if (c >= '0' && c <= '9') return c - '0';
                    if (c >= 'a' && c <= 'f') return c - 'a' + 10;
                    if (c >= 'A' && c <= 'F') return c - 'A' + 10;
                    return 0;
                };
                decoded += char(hexv(mountPoint[i + 1]) * 16
                                + hexv(mountPoint[i + 2]));
                i += 2;
            } else {
                decoded += mountPoint[i];
            }
        }
        mountPoint = decoded;

        if (mountPoint.rfind(dev, 0) == 0 &&
            (mountPoint.size() == dev.size() ||
             mountPoint[dev.size()] == ' ' ||
             mountPoint[dev.size()] == '/'))
            mounts.push_back(mountPoint);
    }
    return mounts;
}

bool diskHasSwap(const std::string& dev)
{
    std::ifstream f("/proc/swaps");
    std::string line;
    std::getline(f, line);            /* header */
    while (std::getline(f, line)) {
        std::istringstream is(line);
        std::string path;
        is >> path;
        if (path.rfind(dev, 0) == 0)
            return true;
    }
    return false;
}

bool runningAsRoot()
{
    return ::geteuid() == 0;
}

bool isBlockDevice(const std::string& path)
{
    struct stat st{};
    return ::stat(path.c_str(), &st) == 0 && S_ISBLK(st.st_mode);
}

} // namespace prism
