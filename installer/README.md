# prism-installer

A **Calamares-like** installer for PrismOS, in **C++ / Qt Widgets**, with
a **CLI mode** (`--cli`). It installs the system's complete boot chain
onto a disk (or an image):

```
LBA 0            protective MBR: boot0 code (patched boot1 LBA)
LBA 1..33        GPT header + 128 entries (+ backup at the end of the disk)
LBA 2048..       P1 "PrismOS ESP"    - FAT32: EFI/BOOT/BOOTX64.EFI (UEFI),
                                          Lua scripts, loader.conf and KERNEL
                  ### unallocated reserve ###  (default: 128 MiB)
                  P2 "PrismOS boot"   - boot1 (patched) + 64-bit BIOS loader
                  P3 "PrismOS data"   - ext4, mounted on / by the kernel
```

The **ESP is the first partition** (P1) on purpose: some OEM firmwares
only try the default boot path (`EFI/BOOT/BOOTX64.EFI`) on the **first**
partition of a removable medium - with the BIOS boot partition first, a
perfectly installed USB drive could still never appear bootable on such
machines. The BIOS boot partition does not need a fixed position: its
actual LBA is patched into `boot0`/`boot1` at install time, and the
loader finds the ESP by scanning the GPT for its type GUID.

The **reserve** is the unallocated space inserted **right after the
ESP**: it is the kernel's growth margin. If a future kernel update
outgrows the ESP's capacity, the ESP will simply be enlarged into this
free area - without moving the other partitions.

The produced layout is identical to the one from `tools/mkimage.py`
(same LBAs, same boot0/boot1 patches, same type GUIDs), which means an
installed image boots exactly like `build/prismos.img`: BIOS **and**
UEFI, kernel loaded from the same FAT32.

## Features

- **One binary, two interfaces**: a Qt graphical wizard (step panel,
  live validation, disk layout bar, real-time log) and a `--cli` text
  wizard (ANSI colors, progress bar, non-interactive `--yes` mode).
- **Administrator privileges requested automatically**: as soon as a
  real disk is targeted, the installer relaunches itself via `pkexec`
  (desktop authentication dialog, like Calamares) or `sudo` (prompt in
  the terminal) - no need to think about `sudo` at launch; test images
  trigger nothing.
- **Installation engine with no Qt dependency**: the GUI runs it in a
  thread; the CLI calls it directly. Reusable and testable.
- **Native FAT32 writer**: the ESP is created entirely in C++ (BPB,
  FSInfo, two FATs, LFN long names, full directory tree) - neither
  `mkfs.fat` nor `mtools` is required.
- **ext4 via `mke2fs`** (e2fsprogs), like the project's `make data`; on
  a real disk the formatting targets the partition node, on an image
  it works at an `offset` with an exact size in blocks.
- **Safety**: mounted/swap disks are refused, root check, target =
  whole disk (never a partition), explicit confirmation, wipe of old
  signatures, **post-install verification** (GPT re-read and CRC
  checked, ESP re-walked, every file checked by CRC32 through a FAT
  reader independent of the writer).
- **Guaranteed UEFI chain**: the `EFI/BOOT/BOOTX64.EFI` boot file is
  required at the source AND validated (PE32+ x86-64, "EFI
  application" subsystem), then re-read from the media after
  installation; `--check` audits an already-installed USB drive in
  read-only mode, and `--efi-boot-entry` creates a boot entry for
  firmwares that treat the USB drive as a fixed disk.
- **Simulation**: `--dry-run` validates everything (sources, plan,
  available space) without writing a single byte.

## Building

Prerequisites: a C++20 compiler (g++ >= 11 or clang >= 14), CMake >=
3.16, Qt 6 (recommended) or Qt 5 (`qt6-base-dev` / `qtbase5-dev`
packages).

```sh
cmake -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
# -> build/prism-installer
```

Or simply:

```sh
./build.sh          # configure + build + runs the CLI test suite
```

Qt installed outside the standard paths:

```sh
cmake -B build -DCMAKE_PREFIX_PATH=$HOME/Qt/6.8.3/gcc_64
```

## Usage

### Preparing the artifacts

The installer consumes the PrismOS build output - run `make` at the
root of the repository, then point the installer at `build/`:

```
build/boot0.bin   build/boot1.bin   build/loader.bin
build/esp/         EFI/BOOT/BOOTX64.EFI, boot/lua/*.lua,
                   boot/defaults/loader.conf, boot/loader.conf,
                   boot/kernel/kernel        <- the kernel
```

The kernel is **required**: the installer checks for its presence
(`esp/boot/kernel/kernel`) and its size is taken into account to
validate the ESP size.

### Administrator privileges

Writing to a real disk (`/dev/sdX`, `/dev/nvme0n1`) requires root
privileges: **the installer requests them itself** at the right moment
and relaunches itself with the same command line -

- **graphical mode**: desktop authentication dialog via `pkexec`
  (polkit, the mechanism used by Calamares); failing that, `sudo -A`
  with a `zenity`/`kdialog` input box, then `sudo` in the original
  terminal. The display variables (`DISPLAY`, `XAUTHORITY`,
  `WAYLAND_DISPLAY`...) are passed on to the elevated process;
- **text mode**: `sudo` asks for the password in the terminal.

No elevation takes place for help/version, **image files** (no
privileges required), the GUI self-test, a non-interactive standard
input (scripts, CI), or when `PRISM_INSTALLER_NO_ELEVATE=1` is set.
If no elevation tool is available, the program exits cleanly with
code `4` and an explicit message.

### Graphical interface

```sh
./build/prism-installer
```

The privilege request appears at startup (desktop dialog). The wizard
has six pages: Welcome -> Artifacts -> Disk -> Partitioning -> Summary
("the disk will be wiped" confirmation) -> Installation (progress +
log) -> Done. Without a display server, the program falls back
cleanly to text mode.

### CLI mode

```sh
./build/prism-installer --cli
```

Interactive wizard: build detection (`build/`, `../build/`), disk
list (model, size, removable), editable default sizes, ASCII-art
plan, confirmation by re-typing the device. The privilege request
(sudo password) comes before the wizard whenever an interactive
terminal is available.

Non-interactive mode:

```sh
./build/prism-installer --cli --yes \
    --source /path/to/PrismOS/build \
    --disk /dev/sdX
```

Test safely on an image (no root privileges required):

```sh
./build/prism-installer --cli --yes \
    --source build --disk /tmp/prism.img --image-size 2G
qemu-system-x86_64 -drive file=/tmp/prism.img,format=raw -serial stdio
```

### Options

| Option | Default | Description |
|---|---|---|
| `-s, --source DIR` | - | PrismOS build output (`make`) |
| `-d, --disk PATH` | - | disk (`/dev/sdX`, `/dev/nvme0n1`) or image |
| `--image-size SIZE` | - | size of the image to create if absent (`512M`, `8G`...) |
| `--esp-size MIB` | 64 | size of the FAT32 ESP (min 48) |
| `--reserve MIB` | 128 | unallocated space before ext4 (kernel margin) |
| `--ext4-size MIB` | rest | size of the ext4 partition |
| `--esp-label L` | `PRISMOS` | FAT32 label |
| `--ext4-label L` | `PRISMDATA` | ext4 label |
| `--dry-run` | - | validate everything, write nothing |
| `--check` | - | **read-only audit** of an already-installed target |
| `--efi-boot-entry` | off | create a "PrismOS" boot entry in the UEFI menu (efibootmgr) |
| `-y, --yes` | - | non-interactive mode (requires `--source` and `--disk`) |
| `--cli` | - | text interface |
| `--mke2fs PATH` | `$PATH` | override (tests) |
| `--efibootmgr PATH` | `$PATH` | override (tests) |

Exit codes: `0` success; `1` usage; `2` validation refused; `3`
installation failure (or problems detected by `--check`); `4`
administrator privileges not obtained; `130` interrupted.

### Auditing an already-installed USB drive (`--check`)

"The installation went fine but the USB drive does not boot" is the
most frustrating failure: the UEFI firmware rejects a boot file
**without saying anything**. The `--check` mode audits a target (disk
or image) in read-only mode and answers point by point, in the order
a firmware walks it:

```sh
sudo ./build/prism-installer --cli --check --disk /dev/sda
```

1. protective MBR (signature `0x55AA`, entry `0xEE`);
2. primary **and backup** GPT (signatures, CRC, bounds);
3. EFI System Partition (GUID `C12A7328-...`): the full FAT32 volume -
   BPB, cluster chains, **LFN checksums** (an EDK2 firmware verifies
   them and ignores long names on a mismatch), backup sectors,
   FSInfo, re-read of every file;
4. **removable boot path**: `EFI/BOOT/BOOTX64.EFI` must exist and be
   a valid x86-64 EFI application (PE32+, subsystem 10) - exactly
   what the firmware's `LoadImage()` requires;
5. BIOS boot partition (magic `PRB1`, bootloader).

Exit `0` = bootable, exit `3` = a precise list of defects. If the
verdict is "bootable" but the firmware still ignores the USB drive,
the problem is on the firmware side - see the troubleshooting below.

## Troubleshooting: the USB drive does not boot in UEFI

**The format produced by the installer has been validated end to end
on a real UEFI firmware (EDK2/OVMF, under QEMU) - with the REAL
PrismOS artifacts**: GPT -> ESP -> FAT32 -> `EFI/BOOT/BOOTX64.EFI` ->
**the bootloader itself** (Lua menu "Welcome to PrismOS", countdown,
kernel loading, "Booting..." captured on screen). The **BIOS chain**
was validated symmetrically (SeaBIOS: boot0 -> boot1 -> bootloader ->
menu -> kernel, COM1 serial console captured). The same
`mkimage.py` reference image boots identically (verified parity). So
if your USB drive does not boot, the problem is almost always on the
**machine's firmware** side, not on the media.
Reproducible test bench: `scripts/ovmf/` in the repository (see
below).

Since the 2026-10-18 version, the installer **detects the Secure Boot
state** (`/sys/firmware/efi/efivars`): an explicit warning is shown
at the end of installation (GUI and CLI) and in `--check` when it is
enabled - it is the #1 cause, and it is now named explicitly.

**Decisive differential diagnosis - "it works under QEMU/OVMF but not
on my PC"**: attaching the physical USB drive to QEMU
(`-drive file=/dev/sdX,format=raw,if=none,id=d -device qemu-xhci
-device usb-storage,drive=d,bootindex=1`) and watching it boot proves
that **the USB drive's content is good** - the GPT table, ESP, FAT32
and BOOTX64.EFI are readable and executable by a real EDK2 firmware
reading exactly the same bytes. Caution: QEMU goes through the
**Linux** USB stack, not the firmware's - this test says nothing
about the firmware's USB enumeration or its menu policy. What
remains is the boot mode (point 2), menu visibility and the USB layer
(points 3 and 4), and the 32-bit firmware as a last resort (point 5).

Since the 2026-10-18 version (evening):
- the ESP's GPT entry carries the **"legacy BIOS bootable" attribute**
  (bit 2, UEFI spec section 5.3) - the GPT equivalent of the MBR
  "active" flag. Compliant firmwares ignore it (OVMF boots with or
  without it, revalidated), but some OEM BIOSes (Insyde/AMI) list a
  GPT disk in the boot menu **only if a partition carries it** -
  without it, the USB drive is invisible even though it boots
  everywhere else;
- the installer **detects media that report themselves as "fixed
  disks"** (sysfs `removable` bit = 0) and recommends
  `--efi-boot-entry` in that case; the end-of-installation reminders
  also cover the "the USB drive does not appear in the menu AT ALL"
  case (full shutdown, USB2 port, Fast Boot disabled, USB Boot
  enabled).

Since the 2026-10-18 version (night): **the ESP is the FIRST
partition (P1)**. Some OEM firmwares only try the default boot path
(`EFI/BOOT/BOOTX64.EFI`) on the **first partition** of a removable
medium - with the previous layout (BIOS boot partition first), a
perfectly installed and verified USB drive could still never boot on
those machines (the drive is not even listed, or is skipped). The
BIOS boot partition now sits after the reserve: nothing to
re-configure, its LBA is patched into boot0/boot1 at install time and
the loader finds the ESP by scanning the GPT. `--check` on an older
USB drive now reports "the ESP is partition P2, not P1" - reinstall
to get the ESP first.

In order of statistical probability:

1. **Secure Boot** is active: the PrismOS boot file is **not
   signed**, the firmware rejects it - *measured on OVMF Secure Boot:
   "Access Denied" then "No bootable option or device was found"; the
   USB drive looks dead while it is perfect*. Disable Secure Boot in
   the setup (F2/Esc at startup -> "Security" menu; on some machines
   you must first set a supervisor password before the option
   appears). *(If Secure Boot is already disabled on your machine,
   go straight to point 2 - the next two cover the vast majority of
   remaining cases.)*
2. **Boot menu / boot mode**: open the menu at startup (F12, Esc, F8,
   F11 depending on the machine) and pick the USB drive's **UEFI**
   entry ("UEFI: ...", often named after the volume: "UEFI:
   PRISMOS" - not "USB HDD: ...", the legacy entry that launches the
   BIOS chain). **If only the "USB HDD" entry appears**, the setup's
   Boot Mode is set to "Legacy" and/or CSM ("Launch CSM") is active:
   the firmware will NEVER look for `EFI/BOOT/BOOTX64.EFI`. Switch
   Boot Mode to UEFI and disable CSM, then retest (the USB drive's
   BIOS chain also works - validated under SeaBIOS - but UEFI is
   what you want).
3. **The USB drive is treated as a fixed disk**: some firmwares only
   try the `\EFI\BOOT\BOOTX64.EFI` path for genuinely removable media
   (the device's "removable" bit). Since the 2026-10-18 version
   (evening), the installer detects this case
   (`/sys/block/*/removable` = 0) and reports it. Create an explicit
   boot entry:
   `sudo ./build/prism-installer --cli --efi-boot-entry -s SOURCE -d /dev/sdX`
   (requires the `efibootmgr` package; the entry lives in THE
   machine's NVRAM - do it from the machine that must boot the USB
   drive). Note: your QEMU test without `removable=on` presents the
   USB drive exactly as a fixed disk - and OVMF still boots it,
   because EDK2 tries the default path on every filesystem; OEM
   firmwares are often stricter, hence the NVRAM entry.
4. **The USB drive does not appear in the boot menu AT ALL**:
   - **full shutdown** of the machine (not a restart), USB drive
     plugged into a **USB2 port** directly on the motherboard (no
     hub or adapter), then power on again - a firmware's USB
     enumeration is sometimes temperamental on a warm restart;
   - in the setup (F2): **"USB Boot" enabled** and **"Fast Boot"
     disabled** (fast boot skips USB initialization);
   - reinstall with the 2026-10-18 version (night): the ESP becomes
     the FIRST partition (P1 - some firmwares only boot from it) and
     carries the "legacy BIOS bootable" attribute designed for
     firmwares that do not list a GPT disk without a boot flag (the
     `--check` of an older USB drive will tell you if either is
     missing);
   - decisive test: the same USB drive on **another PC** - if it
     boots there, the media is definitively out of the picture and
     the culprit is that machine's USB layer or menu policy (also
     try another USB drive on this machine).
5. **32-bit firmware** (rare): `BOOTX64.EFI` does not load on an
   IA32 firmware - a `BOOTIA32.EFI` would be needed.

The `--check` mode distinguishes these scenarios from a real media
defect: if the audit fails, the message says exactly WHAT and WHERE;
if it succeeds while the machine refuses to boot, the culprit is in
the list above (and the machine's Secure Boot state is reported
again).

## UEFI test bench (QEMU + OVMF)

`scripts/ovmf/` (shipped with the project, outside the standard test
suite) reproduces the complete boot chain without hardware:

- `make_hello_efi.py` - generates a **real x86-64 EFI application**
  (PE32+, subsystem 10) containing real machine code: it prints
  "PRISM ESP BOOT OK" via `ConOut->OutputString` then halts -
  observable on screen;
- `run_ovmf_boot_test.py` - attaches an image as a **removable USB
  drive** (`qemu-xhci` + `usb-storage,removable=on`), starts OVMF,
  waits, captures the screen through QMP;
- the image under test is produced by the installer itself (image
  mode) with the above test boot file as `BOOTX64.EFI`.

Measured results (2026-10-18): the installer image **boots**; the
`mkimage.py` image **boots** (parity); the same installer image
under OVMF **Secure Boot** (`secboot` build + `snakeoil` variables)
is **refused** - exactly reproducing the "installed but not
bootable" symptom.

**Reinforced validation of 2026-10-18 (real artifacts)**: the
complete chain was rebuilt from the upstream sources (boot0, boot1,
loader.bin, loader.efi - clang/lld-link, repository Makefile) and
installed by the installer onto a 4 GiB image. OVMF (removable USB)
runs the real `BOOTX64.EFI`: ASCII "PRISMOS" logo, "Welcome to
PrismOS" menu with 7 entries, autoboot countdown, kernel loading,
"Booting..." - captured on screen. SeaBIOS (removable USB, boot0 ->
boot1 -> BIOS bootloader chain) shows the same menu then the kernel
loading on the COM1 serial console. Concretely: **a USB drive
installed by this installer really boots, in BIOS as well as in
UEFI, on EDK2 firmware** - the remaining causes of failure are on
the machine's setup side (see the troubleshooting above).

**Non-regression of 2026-10-18 (evening, bit 2 attribute)**: the
image rebuilt with the "legacy BIOS bootable" attribute on the ESP
boots identically under OVMF (capture: menu, autoboot, "Loading
kernel /boot/kernel/kernel... Booting...") - the attribute adds
visibility for OEM firmwares without changing anything for EDK2.

## Design choices

- **"legacy BIOS bootable" attribute on the ESP** (bit 2 of the GPT
  flags, added on 2026-10-18 evening): superfluous for compliant
  firmwares, decisive for some OEM BIOSes (Insyde/AMI) that only
  display GPT disks carrying a boot flag in the boot menu - the GPT
  equivalent of the MBR "active flag", consistent with the 0xEE
  protective entry already marked 0x80. No measured side effect
  (OVMF, 28-test suite, Python verifier).
- **Homegrown FAT32 instead of mkfs.fat/mtools**: upstream uses
  mtools to *build images*; an installer that writes the FAT32
  itself imposes no dependency and keeps full control (deterministic
  geometry, long names, per-file progress). The geometry follows the
  Microsoft/dosfstools table (512-byte clusters below 128 MiB, etc.)
  and respects the FAT32 minimum of 65,525 clusters.
- **ext4 delegated to mke2fs**: writing a correct ext4 (checksums,
  64-bit descriptors, flex_bg...) is e2fsprogs' job; it is also
  Calamares' choice. On images, `mke2fs -E offset=N ... <blocks>`
  strictly bounds the area so the backup GPT is never touched.
- **Random UUIDs** for the disk and the partitions (the deterministic
  `uuid5` of `mkimage.py` suits an image, not multiple
  installations).
- **Automatic elevation instead of a "relaunch with sudo" message**:
  an installer that fails with "Permission denied" and pushes the
  burden back onto the user is not "clean". The binary therefore
  relaunches itself (`execv`, no useless memory copy) via the tool
  suited to the context - `pkexec env DISPLAY=... XAUTHORITY=...`
  for the GUI (Calamares does the same from its .desktop file),
  `sudo` for the CLI - passing `argv` through unchanged: spaces and
  accents in paths included. Since `execv` replaces the process, a
  user refusal directly returns the `sudo`/`pkexec` exit code (126
  for a cancelled polkit dialog). The decision to elevate is
  deliberately conservative: `/dev`... target, GUI without a target,
  or interactive terminal - never for an image, which requires no
  privileges and would only have the drawback of being owned by
  root.
- **Independent verifier**: `src/Verifier.cpp` shares no code with
  `src/Fat32.cpp` - it re-interprets the on-disk structures. Same
  for `tests/verify_image.py` (Python), used by the test suite as a
  cross-check.
- **UEFI chain verified end to end**: a firmware that does not find
  (or cannot load) `EFI/BOOT/BOOTX64.EFI` makes the USB drive
  unbootable **without the slightest message**. The installer
  therefore requires a valid boot file at the source (presence +
  PE/COFF header: PE32+, x86-64, "EFI application" subsystem - see
  `src/Pe.cpp`), re-reads it from the media after installation, and
  the `--check` mode (`src/BootCheck.cpp`) audits any existing USB
  drive in read-only mode. Real regression: "the installation works
  but it is not bootable, I am in UEFI" - the test artifacts
  themselves contained a BOOTX64.EFI of random bytes that nothing
  distinguished from a real boot file.
- **LFN checksums**: an EDK2 firmware confronts each set of long
  names with the checksum of its 8.3 entry and **ignores the long
  name on a mismatch** (the file becomes visible under its short
  alias only). The C++ verifier and the Python cross-check now
  verify this checksum, just like the firmware.
- **Defensive write order**: the ext4 formatting (mke2fs -
  discard/UNMAP included) and, on a real disk, the re-read of the
  partition table by the kernel (BLKRRPART) are performed BEFORE
  writing the boot code and the ESP. Nothing then comes between
  those writes, their `fsync`, and their control re-read: some USB
  media lose sectors written shortly before an UNMAP, which made
  the verification random. After the `fsync`, the page cache is
  dropped (`posix_fadvise` DONTNEED) so that the verification
  re-reads the media itself.
- **Dense writes on dirty media**: EVERY byte re-read by a compliant
  FAT reader is explicitly rewritten - directory cluster tails
  zeroed (end of directory = 0x00), file cluster tails, volume
  reserved sectors. Real regression: only directory ENTRIES were
  written and, on an already-used USB drive, the end of the root
  directory kept old bytes (leftover machine code from a previous
  flash); the verification then failed with "unexpected non-ASCII
  LFN" in a perfectly deterministic way - the test images,
  implicitly zeroed, masked the defect. Test T16 (media pre-filled
  with a pattern imitating non-ASCII LFN entries) now locks this
  behavior in.

## Tests

No root privileges, no real disk, no e2fsprogs required - the suite
runs on images with a `mke2fs` stub:

```sh
./build.sh                    # or:
sh tests/run_cli_tests.sh
```

Coverage: complete installation + Python cross-verification, strict
dry run (no writes), rejection of corrupted artifacts (boot1 magic),
rejection of too-small ESP/disk, reinstallation over an existing
image, alternate parameters (128 MiB ESP / 0 reserve), custom
labels, interactive prompts (Enter = default, redirected stdin),
closed standard input, **dirty media** (T16: image pre-filled with a
pattern where every 32-byte slot imitates a non-ASCII LFN entry -
the slightest residual byte exposed after a directory entry makes
the installation fail), **UEFI chain** (T17: ESP tree without
`EFI/BOOT/BOOTX64.EFI` => refusal BEFORE any write; T18: a
BOOTX64.EFI that is not a PE => refusal with an explicit message;
T19: `--check` on a compliant image => exit 0 without elevation;
T20: corrupted PE boot file in the image => `--check` exit 3 with a
diagnostic; T21: `--efi-boot-entry` duly invokes `efibootmgr
--create --disk ... --part 1 --loader \EFI\BOOT\BOOTX64.EFI`; T22:
disk without GPT => diagnostic). The test artifacts embed a **real
synthetic EFI application** (PE32+/x86-64/subsystem 10, generated
by `tests/make_test_artifacts.py`).

Privilege elevation (exercised by BOTH binaries - the
`cli_main.cpp` test binary embeds the same layer as `main.cpp`):
sudo missing => code 4, `/dev` target => relaunch via `sudo` with
`argv` intact (stub), image target => no call (stub present but not
invoked), graphical mode => `pkexec env DISPLAY=...` with the
binary's absolute path (stub), `PRISM_INSTALLER_NO_ELEVATE=1` =>
bypass honored.

The graphical mode has a built-in self-test (used in CI):

```sh
# QT_QPA_PLATFORM=offscreen is enough: an explicit platform counts
# as a display (the text fallback does not kick in).
QT_QPA_PLATFORM=offscreen ./build/prism-installer --gui-self-test \
    --source build --disk /tmp/t.img --image-size 512M --mke2fs tests/.../mke2fs
```

## Structure

```
installer/
|-- CMakeLists.txt         Qt6/Qt5 + AUTOMOC
|-- build.sh               configure + build + tests
|-- src/
|   |-- main.cpp           GUI / CLI dispatch (text fallback without a display)
|   |-- Cli.{h,cpp}        text wizard, option parsing (shared)
|   |-- Elevate.{h,cpp}    administrator privilege request (pkexec/sudo)
|   |-- Gui.{h,cpp}        Qt Widgets wizard (worker thread, LayoutBar)
|   |-- Installer.{h,cpp}  orchestrator: validation -> plan -> write
|   |-- Gpt.{h,cpp}        protective MBR, GPT, boot0/boot1 patches
|   |-- Fat32.{h,cpp}      native FAT32 writer (streaming pwrite)
|   |-- Ext4.{h,cpp}       mke2fs formatting (disk or image offset)
|   |-- Verifier.{h,cpp}   post-install verification (independent FAT reader)
|   |-- Pe.{h,cpp}         PE/COFF header parsing (UEFI boot file)
|   |-- BootCheck.{h,cpp}  --check audit of a target (read-only)
|   |-- DiskScan.{h,cpp}   /sys/block, mounts, swap, permissions
|   |-- Process.{h,cpp}    fork/exec with capture
|   |-- Util.{h,cpp}       CRC32, UUID v4/v5 (SHA-1), human-readable sizes
|   |-- Layout.h           shared constants (mirror of prism.inc)
|   `-- Version.h
`-- tests/
    |-- cli_main.cpp       CLI binary without Qt (engine tests)
    |-- make_test_artifacts.py  deterministic synthetic artifacts
    |                       (including a synthetic x86-64 EFI application)
    |-- mke2fs_stub.py     mke2fs stub (call log)
    |-- verify_image.py    independent verifier (MBR/GPT/FAT32/ext4/PE)
    `-- run_cli_tests.sh   the suite (28 checks)
```

## License

GPL-3.0-or-later, like PrismOS.
