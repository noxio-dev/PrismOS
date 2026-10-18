/*-
 * SPDX-License-Identifier: GPL-3.0-or-later
 * Copyright (c) 2026 noxio-dev
 *
 * prism-installer - GPT/MBR implementation.
 */

#include "Gpt.h"
#include "Layout.h"

#include <cstring>
#include <stdexcept>

namespace prism {

/* ---- Little-endian helpers -------------------------------------------- */

namespace {

void putLe16(std::vector<uint8_t>& v, size_t off, uint16_t x)
{
    v[off]     = uint8_t(x);
    v[off + 1] = uint8_t(x >> 8);
}

void putLe32(std::vector<uint8_t>& v, size_t off, uint32_t x)
{
    v[off]     = uint8_t(x);
    v[off + 1] = uint8_t(x >> 8);
    v[off + 2] = uint8_t(x >> 16);
    v[off + 3] = uint8_t(x >> 24);
}

void putLe64(std::vector<uint8_t>& v, size_t off, uint64_t x)
{
    for (int i = 0; i < 8; ++i)
        v[off + size_t(i)] = uint8_t(x >> (8 * i));
}

void putName(std::vector<uint8_t>& v, size_t off, const std::string& name)
{
    /* GPT partition name: UTF-16LE, 72 bytes, zero-padded. */
    for (size_t i = 0; i < 36; ++i) {
        uint16_t ch = i < name.size() ? uint8_t(name[i]) : 0;
        putLe16(v, off + i * 2, ch);
    }
}

} // namespace

/* ---- Artifact validation ----------------------------------------------- */

std::string BootArtifacts::validate() const
{
    if (boot0.size() != SECTOR)
        return "boot0 must be exactly 512 bytes ("
               + std::to_string(boot0.size()) + " found)";
    if (boot1.size() != BOOT1_SECTORS * SECTOR)
        return "boot1 must be exactly "
               + std::to_string(BOOT1_SECTORS * SECTOR) + " bytes ("
               + std::to_string(boot1.size()) + " found)";
    if (util::readLe32(boot1, 0) != BOOT1_MAGIC)
        return "boot1: wrong magic (expected 0x31425250 \"PRB1\")";
    if (loader.size() == 0)
        return "the BIOS loader image (loader.bin) is empty";
    if (loader.size() > LOADER_MAX_SIZE)
        return "the loader exceeds LOADER_MAX_SIZE (4 MiB): "
               + std::to_string(loader.size()) + " bytes";
    if (loader.size() > (BOOT_PART_SECTORS - BOOT1_SECTORS) * SECTOR)
        return "boot1 + loader do not fit in the 2 MiB "
               "boot partition";
    return "";
}

/* ---- Protective MBR ----------------------------------------------------- */

std::vector<uint8_t> buildProtectiveMbr(const BootArtifacts& art,
                                        const PartitionPlan& plan)
{
    std::vector<uint8_t> mbr = art.boot0;      /* 512 bytes of boot0 code */

    /* Patch expected by boot0: (u64) LBA of boot1 in the boot
     * partition.  BOOT0_LBA_OFF = 0x1B0, the DAP lives at 0x1A8. */
    putLe64(mbr, BOOT0_LBA_OFF, plan.bootStart);

    /* Disk signature (identical to mkimage.py). */
    putLe32(mbr, 0x1B8, MBR_DISK_SIG);

    /* Single protective partition entry: type 0xEE. */
    uint64_t sectors = plan.totalSectors - 1;
    if (sectors > 0xFFFFFFFFull) sectors = 0xFFFFFFFFull;

    mbr[0x1BE] = 0x80;                       /* bootable (as upstream)   */
    mbr[0x1BF] = 0x00; mbr[0x1C0] = 0x02; mbr[0x1C1] = 0x00;  /* start CHS */
    mbr[0x1C2] = 0xEE;                       /* protective GPT type      */
    mbr[0x1C3] = 0xFF; mbr[0x1C4] = 0xFF; mbr[0x1C5] = 0xFF;  /* end CHS    */
    putLe32(mbr, 0x1C6, 1);                  /* starting LBA             */
    putLe32(mbr, 0x1CA, uint32_t(sectors));  /* sector count             */

    mbr[0x1FE] = 0x55;
    mbr[0x1FF] = 0xAA;
    return mbr;
}

/* ---- GPT entries --------------------------------------------------------- */

std::vector<uint8_t> buildGptEntries(const PartitionPlan& plan)
{
    /* 128 entries x 128 bytes, the rest zero. */
    std::vector<uint8_t> entries(GPT_ENTRIES * 128, 0);

    auto setEntry = [&](size_t idx, const char* typeGuid, const char* name,
                        uint64_t first, uint64_t last, const util::Uuid& part,
                        uint64_t attrs) {
        std::vector<uint8_t> e(128, 0);
        std::vector<uint8_t> type = util::Uuid::parse(typeGuid).bytesLe();
        std::memcpy(e.data() + 0, type.data(), 16);
        std::memcpy(e.data() + 16, part.bytesLe().data(), 16);
        putLe64(e, 32, first);
        putLe64(e, 40, last);
        putLe64(e, 48, attrs);                /* attributes */
        putName(e, 56, name);
        std::memcpy(entries.data() + idx * 128, e.data(), 128);
    };

    /* Entry #0 = ESP (P1): the FIRST partition must be the ESP - some
     * OEM firmwares only try the default boot path
     * (EFI/BOOT/BOOTX64.EFI) on the first partition of removable
     * media. */
    /* ESP: legacy BIOS bootable attribute (bit 2, value 0x4, UEFI spec section 5.3)
     * - the GPT equivalent of the MBR "active" flag.  Ignored by
     * conformant firmwares (OVMF boots with or without it), but some
     * OEM BIOSes (Insyde/AMI) only list a GPT disk in the boot menu
     * (F12) if a partition carries it: without it, the key can be
     * "invisible" even though it boots perfectly elsewhere (QEMU/OVMF
     * for example).  The protective MBR already carries the 0x80 flag
     * for the same reason. */
    setEntry(0, GUID_ESP, PART_NAME_ESP,
             plan.espStart, plan.espStart + plan.espSectors - 1,
             plan.espGuid, 0x4);

    /* Entry #1 = BIOS boot partition (P2), right after the ESP: its
     * LBA is patched into boot0 (MBR) and boot1 at install time. */
    setEntry(1, GUID_BIOS_BOOT, PART_NAME_BOOT,
             plan.bootStart, plan.bootStart + plan.bootSectors - 1,
             plan.bootGuid, 0);

    /* Entry #2 = ext4 root (P3): the loader finds it by its Linux
     * filesystem type GUID, the kernel mounts it on /. */
    setEntry(2, GUID_LINUX_FS, PART_NAME_ROOT,
             plan.rootStart, plan.rootStart + plan.rootSectors - 1,
             plan.rootGuid, 0);

    return entries;
}

/* ---- GPT header ----------------------------------------------------------- */

std::vector<uint8_t> buildGptHeader(const PartitionPlan& plan,
                                    uint32_t entriesCrc, bool backup)
{
    std::vector<uint8_t> h(92, 0);

    std::memcpy(h.data() + 0, "EFI PART", 8);
    putLe32(h, 8, 0x00010000);                /* revision 1.0            */
    putLe32(h, 12, 92);                       /* header size             */
    putLe32(h, 16, 0);                        /* CRC (computed below)    */
    putLe32(h, 20, 0);                        /* reserved                */

    uint64_t myLba   = backup ? plan.lastLba - 32 + 32 : 1;  /* see below  */
    uint64_t my      = backup ? plan.lastLba : 1;
    uint64_t alt     = backup ? 1 : plan.lastLba;
    uint64_t entries = backup ? plan.lastLba - 32 : 2;
    (void)myLba;

    putLe64(h, 24, my);                       /* LBA of this header      */
    putLe64(h, 32, alt);                      /* LBA of opposite header  */
    putLe64(h, 40, plan.firstUsable);
    putLe64(h, 48, plan.lastUsable);

    std::memcpy(h.data() + 56, plan.diskGuid.bytesLe().data(), 16);

    putLe64(h, 72, entries);                  /* LBA of the entries table */
    putLe32(h, 80, GPT_ENTRIES);              /* number of entries        */
    putLe32(h, 84, 128);                      /* size of one entry        */
    putLe32(h, 88, entriesCrc);               /* CRC of the table         */

    putLe32(h, 16, util::crc32(h.data(), 92));
    return h;
}

/* ---- BIOS boot partition --------------------------------------------------- */

std::vector<uint8_t> buildBootPartition(const BootArtifacts& art,
                                        const PartitionPlan& plan)
{
    std::vector<uint8_t> part(BOOT_PART_SECTORS * SECTOR, 0);

    /* boot1 with its patched header: loader LBA (u64 @ 4, i.e.
     * hdr_lba_lo/hi) and loader sector count (u32 @ 12). */
    std::vector<uint8_t> boot1 = art.boot1;
    uint64_t loaderLba = plan.bootStart + BOOT1_SECTORS;
    uint64_t loaderSectors = (art.loader.size() + SECTOR - 1) / SECTOR;

    putLe64(boot1, 4, loaderLba);
    putLe32(boot1, 12, uint32_t(loaderSectors));

    std::memcpy(part.data(), boot1.data(), boot1.size());
    std::memcpy(part.data() + boot1.size(), art.loader.data(), art.loader.size());
    return part;
}

} // namespace prism
