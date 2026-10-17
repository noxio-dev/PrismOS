/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - utilities implementation.
 */

#include "Util.h"
#include "Layout.h"

#include <array>
#include <chrono>
#include <cstdio>
#include <cstring>
#include <fstream>
#include <random>
#include <sstream>
#include <stdexcept>

#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

namespace prism::util {

/* ---- CRC32 ------------------------------------------------------------ */

uint32_t crc32(const uint8_t* data, size_t len)
{
    /* Lazily computed table (IEEE, poly 0xEDB88320, reflected). */
    static std::array<uint32_t, 256> table = [] {
        std::array<uint32_t, 256> t{};
        for (uint32_t n = 0; n < 256; ++n) {
            uint32_t c = n;
            for (int k = 0; k < 8; ++k)
                c = (c & 1) ? (0xEDB88320u ^ (c >> 1)) : (c >> 1);
            t[n] = c;
        }
        return t;
    }();

    uint32_t crc = 0xFFFFFFFFu;
    for (size_t i = 0; i < len; ++i)
        crc = table[(crc ^ data[i]) & 0xFF] ^ (crc >> 8);
    return crc ^ 0xFFFFFFFFu;
}

/* ---- UUID -------------------------------------------------------------- */

Uuid Uuid::nil()
{
    Uuid u{};
    std::memset(u.b, 0, 16);
    return u;
}

Uuid Uuid::randomV4()
{
    Uuid u{};
    bool ok = false;

    std::ifstream urand("/dev/urandom", std::ios::binary);
    if (urand) {
        urand.read(reinterpret_cast<char*>(u.b), 16);
        ok = urand.gcount() == 16;
    }
    if (!ok) {
        /* Deterministic-in-any-case fallback, good enough for a disk. */
        std::random_device rd;
        std::mt19937_64 gen(rd());
        for (int i = 0; i < 2; ++i) {
            uint64_t v = gen();
            std::memcpy(u.b + i * 8, &v, 8);
        }
    }

    u.b[6] = (u.b[6] & 0x0F) | 0x40;   /* version 4 */
    u.b[8] = (u.b[8] & 0x3F) | 0x80;   /* variant RFC 4122 */
    return u;
}

/* Minimal SHA-1 (RFC 3174) - needed for UUID v5. */
namespace {

struct Sha1 {
    uint32_t h[5] = {0x67452301, 0xEFCDAB89, 0x98BADCFE,
                     0x10325476, 0xC3D2E1F0};
    uint64_t total = 0;
    uint8_t buf[64]{};
    size_t bufLen = 0;

    static uint32_t rotl(uint32_t x, int n) { return (x << n) | (x >> (32 - n)); }

    void block(const uint8_t* p)
    {
        uint32_t w[80];
        for (int i = 0; i < 16; ++i)
            w[i] = (uint32_t(p[i * 4]) << 24) | (uint32_t(p[i * 4 + 1]) << 16) |
                   (uint32_t(p[i * 4 + 2]) << 8) | uint32_t(p[i * 4 + 3]);
        for (int i = 16; i < 80; ++i)
            w[i] = rotl(w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16], 1);

        uint32_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4];
        for (int i = 0; i < 80; ++i) {
            uint32_t f, k;
            if (i < 20)      { f = (b & c) | (~b & d);            k = 0x5A827999; }
            else if (i < 40) { f = b ^ c ^ d;                     k = 0x6ED9EBA1; }
            else if (i < 60) { f = (b & c) | (b & d) | (c & d);   k = 0x8F1BBCDC; }
            else             { f = b ^ c ^ d;                     k = 0xCA62C1D6; }
            uint32_t t = rotl(a, 5) + f + e + k + w[i];
            e = d; d = c; c = rotl(b, 30); b = a; a = t;
        }
        h[0] += a; h[1] += b; h[2] += c; h[3] += d; h[4] += e;
    }

    void update(const uint8_t* p, size_t n)
    {
        total += n;
        while (n) {
            size_t take = std::min(n, sizeof(buf) - bufLen);
            std::memcpy(buf + bufLen, p, take);
            bufLen += take; p += take; n -= take;
            if (bufLen == sizeof(buf)) {
                block(buf);
                bufLen = 0;
            }
        }
    }

    std::array<uint8_t, 20> finish()
    {
        uint64_t bits = total * 8;
        uint8_t pad = 0x80;
        update(&pad, 1);
        uint8_t zero = 0;
        while (bufLen != 56) update(&zero, 1);
        uint8_t lenb[8];
        for (int i = 0; i < 8; ++i) lenb[i] = uint8_t(bits >> (56 - i * 8));
        update(lenb, 8);
        std::array<uint8_t, 20> out{};
        for (int i = 0; i < 5; ++i) {
            out[i * 4]     = uint8_t(h[i] >> 24);
            out[i * 4 + 1] = uint8_t(h[i] >> 16);
            out[i * 4 + 2] = uint8_t(h[i] >> 8);
            out[i * 4 + 3] = uint8_t(h[i]);
        }
        return out;
    }
};

std::vector<uint8_t> hexToBytes(const std::string& hex)
{
    auto nib = [](char c) -> int {
        if (c >= '0' && c <= '9') return c - '0';
        if (c >= 'a' && c <= 'f') return c - 'a' + 10;
        if (c >= 'A' && c <= 'F') return c - 'A' + 10;
        return -1;
    };
    std::vector<uint8_t> out;
    int hi = -1;
    for (char c : hex) {
        if (c == '-' || c == '{' || c == '}' || c == ' ') continue;
        int v = nib(c);
        if (v < 0) throw std::runtime_error("invalid UUID: " + hex);
        if (hi < 0) hi = v;
        else { out.push_back(uint8_t((hi << 4) | v)); hi = -1; }
    }
    if (out.size() != 16) throw std::runtime_error("invalid UUID: " + hex);
    return out;
}

} // namespace

Uuid Uuid::nameV5(const Uuid& ns, const std::string& name)
{
    Sha1 sha;
    sha.update(ns.b, 16);
    sha.update(reinterpret_cast<const uint8_t*>(name.data()), name.size());
    auto digest = sha.finish();

    Uuid u{};
    std::memcpy(u.b, digest.data(), 16);
    u.b[6] = (u.b[6] & 0x0F) | 0x50;   /* version 5 */
    u.b[8] = (u.b[8] & 0x3F) | 0x80;
    return u;
}

Uuid Uuid::parse(const std::string& s)
{
    auto bytes = hexToBytes(s);
    Uuid u{};
    std::memcpy(u.b, bytes.data(), 16);
    return u;
}

std::string Uuid::str() const
{
    char out[40];
    std::snprintf(out, sizeof(out),
        "%02X%02X%02X%02X-%02X%02X-%02X%02X-%02X%02X-%02X%02X%02X%02X%02X%02X",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]);
    return out;
}

std::vector<uint8_t> Uuid::bytesLe() const
{
    /* GPT stores the first 3 fields in little-endian. */
    std::vector<uint8_t> out(16);
    for (int i = 0; i < 4; ++i)   out[i]      = b[3 - i];
    for (int i = 0; i < 2; ++i)   out[4 + i]  = b[5 - i];
    for (int i = 0; i < 2; ++i)   out[6 + i]  = b[7 - i];
    for (int i = 8; i < 16; ++i)  out[i]      = b[i];
    return out;
}

Uuid prismNamespace()
{
    /* uuid.uuid5(uuid.NAMESPACE_DNS, "prismos.noxio.dev") - same upstream. */
    static const Uuid DNS = Uuid::parse("6ba7b810-9dad-11d1-80b4-00c04fd430c8");
    return Uuid::nameV5(DNS, "prismos.noxio.dev");
}

/* ---- File I/O ------------------------------------------------------------ */

std::vector<uint8_t> readFile(const std::string& path)
{
    std::ifstream f(path, std::ios::binary);
    if (!f)
        throw std::runtime_error("unable to open " + path);

    f.seekg(0, std::ios::end);
    std::streamoff size = f.tellg();
    if (size < 0)
        throw std::runtime_error("unable to determine the size of " + path);
    f.seekg(0, std::ios::beg);

    std::vector<uint8_t> data;
    data.resize(size_t(size));
    if (size > 0)
        f.read(reinterpret_cast<char*>(data.data()), size);
    if (f.gcount() != size)
        throw std::runtime_error("incomplete read of " + path);
    return data;
}

uint64_t readLe64(const std::vector<uint8_t>& v, size_t off)
{
    uint64_t x = 0;
    for (int i = 7; i >= 0; --i) x = (x << 8) | v[off + size_t(i)];
    return x;
}

uint32_t readLe32(const std::vector<uint8_t>& v, size_t off)
{
    return uint32_t(v[off]) | (uint32_t(v[off + 1]) << 8) |
           (uint32_t(v[off + 2]) << 16) | (uint32_t(v[off + 3]) << 24);
}

void fill(std::vector<uint8_t>& v, size_t off, size_t len, uint8_t byte)
{
    if (off + len > v.size())
        throw std::runtime_error("fill out of range");
    std::memset(v.data() + off, byte, len);
}

/* ---- FAT timestamps ------------------------------------------------------ */

FatTime toFatTime(int64_t unixTime)
{
    /* Days since the UNIX epoch -> civil date (Howard Hinnant algorithm). */
    int64_t days = unixTime / 86400;
    int64_t secs = unixTime % 86400;
    if (secs < 0) { secs += 86400; days -= 1; }

    int64_t z = days + 719468;
    int64_t era = (z >= 0 ? z : z - 146096) / 146097;
    int64_t doe = z - era * 146097;
    int64_t yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    int64_t y = yoe + era * 400;
    int64_t doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    int64_t mp = (5 * doy + 2) / 153;
    int64_t d = doy - (153 * mp + 2) / 5 + 1;
    int64_t m = mp < 10 ? mp + 3 : mp - 9;
    if (m <= 2) y += 1;

    int yy = int(y - 1980);
    if (yy < 0) yy = 0;
    if (yy > 127) yy = 127;

    FatTime t{};
    t.date  = uint16_t((yy << 9) | (m << 5) | d);
    t.time  = uint16_t((secs / 3600) << 11 | ((secs / 60) % 60) << 5 | (secs % 60) / 2);
    t.tenth = uint16_t((secs % 2) * 100);
    return t;
}

/* ---- Path types ---------------------------------------------------------- */

bool isBlockDevice(const std::string& path)
{
    struct stat st{};
    return ::stat(path.c_str(), &st) == 0 && S_ISBLK(st.st_mode);
}

bool isRegularFile(const std::string& path)
{
    struct stat st{};
    return ::stat(path.c_str(), &st) == 0 && S_ISREG(st.st_mode);
}

int secureBootEnabled()
{
    /* "SecureBoot" EFI variable of the global EFI GUID (vendor-less):
     * efivarfs format = u32 attributes + payload; here 1 byte. */
    static const char path[] =
        "/sys/firmware/efi/efivars/"
        "SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c";

    std::FILE* f = std::fopen(path, "rb");
    if (!f) return -1;
    uint8_t buf[16];
    size_t n = std::fread(buf, 1, sizeof(buf), f);
    std::fclose(f);
    if (n < 5) return -1;
    return buf[n - 1] ? 1 : 0;
}

} // namespace prism::util

namespace prism {

std::string humanSize(uint64_t bytes)
{
    char buf[64];
    const char* unit;
    double v;
    if (bytes >= 1024ull * 1024 * 1024 * 1024) {
        v = double(bytes) / (1024.0 * 1024 * 1024 * 1024); unit = "TiB";
    } else if (bytes >= 1024ull * 1024 * 1024) {
        v = double(bytes) / (1024.0 * 1024 * 1024); unit = "GiB";
    } else if (bytes >= 1024ull * 1024) {
        v = double(bytes) / (1024.0 * 1024); unit = "MiB";
    } else if (bytes >= 1024ull) {
        v = double(bytes) / 1024.0; unit = "KiB";
    } else {
        v = double(bytes); unit = "B";
    }
    std::snprintf(buf, sizeof(buf), "%.2f %s", v, unit);
    return buf;
}

} // namespace prism
