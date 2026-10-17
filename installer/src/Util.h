/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - utilities: CRC32, UUID, file reading, size
 * display.  No Qt dependency (engine reusable by both the CLI
 * and the GUI).
 */

#pragma once

#include <cstdint>
#include <string>
#include <vector>

namespace prism::util {

/* CRC32 (IEEE 802.3, same as zlib.crc32) - used by the GPT headers. */
uint32_t crc32(const uint8_t* data, size_t len);
inline uint32_t crc32(const std::vector<uint8_t>& v)
    { return crc32(v.data(), v.size()); }

/* Represents a UUID (logical big-endian, as in RFC 4122 strings). */
struct Uuid {
    uint8_t b[16];

    static Uuid nil();
    /* Random UUID v4 (source: /dev/urandom, fallback: clock + pid). */
    static Uuid randomV4();
    /* Deterministic UUID v5 (truncated SHA-1), like tools/mkimage.py. */
    static Uuid nameV5(const Uuid& ns, const std::string& name);
    static Uuid parse(const std::string& s);       /* throws std::runtime_error */
    std::string str() const;
    /* Little-endian representation "as stored on disk" (GPT). */
    std::vector<uint8_t> bytesLe() const;
};

/* DNS namespace prismos.noxio.dev - same as mkimage.py. */
Uuid prismNamespace();

/* Reads a whole file into memory.  Throws std::runtime_error on failure. */
std::vector<uint8_t> readFile(const std::string& path);

/* Reads a 64-bit integer at a given offset in a buffer. */
uint64_t readLe64(const std::vector<uint8_t>& v, size_t off);
uint32_t readLe32(const std::vector<uint8_t>& v, size_t off);

/* Fills the bytes of a range (offset, size) - no reallocation. */
void fill(std::vector<uint8_t>& v, size_t off, size_t len, uint8_t byte);

/* FAT date/time (UTC): encodes a time_t into the DOS fields. */
struct FatTime { uint16_t date; uint16_t time; uint16_t tenth; };
FatTime toFatTime(int64_t unixTime);

/* True if the path designates a block device. */
bool isBlockDevice(const std::string& path);
/* True if the path designates a regular file. */
bool isRegularFile(const std::string& path);

/* Secure Boot state of the CURRENT machine's firmware:
 *   1 = enabled, 0 = disabled, -1 = undetermined (BIOS machine,
 *   efivars inaccessible, container, test).
 * Read from /sys/firmware/efi/efivars (global EFI variable
 * SecureBoot-8be4df61-...): the value byte is the last one in the
 * file (4 attribute bytes + 1 payload byte). */
int secureBootEnabled();

} // namespace prism::util

namespace prism {

/* see Layout.h - implementation here to keep Layout.h free of a dedicated .cpp. */

} // namespace prism
