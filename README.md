# PrismOS

A small experimental x86 operating system, written from scratch:
BIOS bootloader in assembly, then a C (and Rust soon) kernel. Goal: learn, the clean way.

## Boot chain

```
BIOS → boot0 (MBR, 512 bytes, 0x7C00)
     → stage2 (sector 2, 0x7E00)
     → loader → kernel (32-bit protected mode)
```

- **boot0**: boot sector. Flat segments, reads sector 2 via `int 13h`,
  far jumps to stage2.
- **stage2**: proves it's alive, prepares the protected mode switch.
- **loader**: GDT + 32-bit switch.
- **kern**: C kernel (`sys/kern/main.c`).

## Build & run

Requires `gcc` (multilib), `binutils`, `make`, `qemu-system-i386`.

```sh
make            # full build → build/prismos.img
make run        # boot it in QEMU
make clean      # wipe everything
```

The bootable image is `build/prismos.img` (raw format, legacy BIOS boot).

## Layout

```
boot/     bootloader (boot0 = MBR, stage2, loader)
include/  shared headers (prism.inc)
sys/kern/ kernel
Makefile  build: -m32 assembly, elf_i386 linking, raw disk image
```

## License

GPL-3.0-or-later — see [LICENSE](LICENSE).