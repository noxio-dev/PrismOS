# PrismOS

A small experimental x86-64 operating system, written from scratch.
FreeBSD-style boot: a real **BIOS + UEFI bootloader** whose logic is written in
**Lua**, loading a **Rust** kernel (for now we have a shell).

## Todo

- [x] Phase 1.1 — heap (slab/buddy)
- [ ] Phase 1.2 — ACPI/APIC (LAPIC + IOAPIC + 8259 masked)
- [ ] Phase 1.3 — PCI/AHCI
- [ ] Phase 1.4 — VFS + ext4 R/W
- [ ] Phase 2 — ring 3, syscalls, process, scheduler, ELF loader
- [ ] Phase 3 — POSIX syscalls, newlib, BusyBox, shell userspace
- [ ] Phase 4 — cross-binutils + cross-gcc
- [ ] Phase 5 — native gcc -> self-hosting bootstrap

*`One day, perhaps, PrismOS will be self-hosted lol`*

## Boot chain

One disk image boots both ways (GPT with a protective MBR):

| Partition       | Content                                                       |
|-----------------|---------------------------------------------------------------|
| `PrismOS boot`  | raw: boot1 + the 64-bit BIOS loader                           |
| `PrismOS ESP`   | FAT32: `EFI/BOOT/BOOTX64.EFI`, `boot/` (Lua, conf, kernel)    |

The BIOS loader mounts the ESP with its own FAT32 driver, the UEFI loader uses
the firmware's. The Lua scripts and the kernel are therefore the same files.

## Build & run

Requirements: `clang` + `lld` (loader, `lld-link`), `gcc`/`binutils` (boot
sectors), `make`, `python3`, `mkfs.fat` + `mtools`, Rust with the bare-metal
target (`rustup target add x86_64-unknown-none`). To run: `qemu-system-x86_64`
and, for UEFI, OVMF.

```sh
make              # build/prismos.img
make run          # boot it in QEMU (BIOS), serial on stdio
make run-uefi     # boot it in QEMU with OVMF (UEFI)
make test         # automated BIOS + UEFI boot tests through the serial console
make clean
```

Write it to a USB stick with `dd if=build/prismos.img of=/dev/sdX bs=1M`
(it boots on both legacy BIOS and UEFI machines).

## The loader

`boot/loader/` is the common part: ELF64 kernel loader, memory map, page
tables, ANSI terminal, Lua bindings. `boot/bios/` and `boot/efi/` are the two
platform backends (disk/console/memory/firmware tables). `boot/libsa/` is a
tiny freestanding libc (string, printf with floats, malloc, setjmp, math...)
so that stock Lua 5.4 (`contrib/lua`) builds unmodified.

### Lua scripts (`boot/lua/`)

| File         | Role                                                          |
|--------------|---------------------------------------------------------------|
| `loader.lua` | entry point: read config, autoboot or menu                    |
| `config.lua` | `loader.conf` parser, module list, kernel loading, boot       |
| `menu.lua`   | boot menu with autoboot countdown                             |
| `cli.lua`    | `OK` prompt: `boot`, `set`, `show`, `ls`, `more`, `lsmem`, `lua`... |

The scripts only talk to the firmware through the `loader` table: `write`,
`getkey`, `time`, `delay`, `reboot`, `getenv`/`setenv`/`unsetenv`/`environ`,
`readfile`, `readdir`, `stat`, `exists`, `load_kernel`, `load_module`,
`kernel`, `memmap`, `boot`, plus `loader.platform` (`"bios"`/`"uefi"`) and
`loader.version`. Only the `base`, `table`, `string`, `math`, `utf8` and
`coroutine` libraries are available; `require` loads `/boot/lua/<name>.lua`.

### Configuration

`/boot/defaults/loader.conf` then `/boot/loader.conf` (FreeBSD syntax
`name="value"`). Useful variables: `kernel`, `kernel_options`,
`autoboot_delay` (seconds, `NO` = wait), `beastie_disable="YES"` (no menu),
`loader_menu_title`, and modules through `<name>_load="YES"`,
`<name>_name`, `<name>_type`.

The loader also works on a serial console (COM1, 115200 8N1) on BIOS; on UEFI
it uses whatever console the firmware provides.

## Boot protocol

The kernel is an ELF64 executable linked in the top 2 GiB (`KERNBASE =
0xFFFFFFFF80000000`, see `sys/kern/linker.ld`). It is entered in long mode with
paging on (identity map of 4 GiB + the kernel at its link address), interrupts
off, and `rdi` pointing to a `struct prism_bootinfo`: firmware type, memory map
(typed, with loader memory marked reclaimable), modules, command line,
environment, ACPI RSDP, SMBIOS, framebuffer. Definitions:
`include/prism/boot.h` (C) and `sys/kern/src/boot.rs` (Rust).

## Layout

```
boot/boot0, boot/boot1   BIOS sector code (assembly)
boot/bios, boot/efi      platform backends
boot/loader, boot/libsa  loader core, mini libc
boot/lua, boot/conf      Lua scripts and loader.conf
contrib/lua              Lua 5.4.8 (MIT)
include/                 prism.inc (shared constants), prism/boot.h
sys/kern                 Rust kernel (no_std, x86_64-unknown-none)
tools/mkimage.py         GPT/FAT32 disk image builder
tests/boot_test.py       QEMU boot tests
```

## License

GPL-3.0-or-later — see [LICENSE](LICENSE). Lua is MIT licensed, see
`contrib/lua/COPYRIGHT`. The framebuffer font is public domain.
