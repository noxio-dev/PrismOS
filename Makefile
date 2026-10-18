#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#

NAME      := prismos
ifeq ($(origin CC),default)
CC        := clang
endif
AS        := gcc
LD        := ld
LLD_LINK  ?= lld-link
PYTHON    ?= python3
CARGO     ?= cargo
CARGOFLAGS ?=
QEMU      ?= qemu-system-x86_64
DATA_IMG  ?= build/data.img
DATA_SIZE ?= 64M
# Both disks sit on one AHCI controller: the boot disk on port 0 (where the
# kernel finds its ext4 root partition P3) and an extra ext4 volume on port 1,
# kept between runs, that the shell can mount on demand.
QEMU_AHCI := -device ahci,id=ahci0
QEMU_DATA := -drive id=data,file=$(DATA_IMG),format=raw,if=none -device ide-hd,drive=data,bus=ahci0.1
# $(call qemu_boot_drive,image) - the bootable disk, AHCI port 0
qemu_boot_drive = -drive id=boot,file=$(1),format=raw,if=none -device ide-hd,drive=boot,bus=ahci0.0
QEMUFLAGS ?= -m 256M $(QEMU_AHCI) $(QEMU_DATA)
OVMF_CODE ?= $(firstword $(wildcard \
    /usr/share/OVMF/OVMF_CODE_4M.fd \
    /usr/share/OVMF/OVMF_CODE.fd \
    /usr/share/edk2/ovmf/OVMF_CODE.fd \
    /usr/share/qemu/OVMF_CODE.fd \
    /usr/share/edk2/x64/OVMF_CODE.4m.fd))

OVMF_VARS ?= $(firstword $(wildcard \
    /usr/share/OVMF/OVMF_VARS_4M.fd \
    /usr/share/OVMF/OVMF_VARS.fd \
    /usr/share/edk2/ovmf/OVMF_VARS.fd \
    /usr/share/qemu/OVMF_VARS.fd \
    /usr/share/edk2/x64/OVMF_VARS.4m.fd))

XORRISO     ?= xorriso
MKFS_FAT    ?= mkfs.fat
MCOPY       ?= mcopy
ESPIMG_SIZE ?= 32M

B         := build
CCRES     := $(shell $(CC) -print-resource-dir)/include

COMMON_CFLAGS := -ffreestanding -fno-stack-protector -fno-asynchronous-unwind-tables \
                 -fno-math-errno -mno-red-zone -O2 -g0 -Wall -Wextra \
                 -Wno-unused-parameter -Wno-sign-compare -Wno-missing-field-initializers \
                 -nostdinc -isystem boot/libsa/include -isystem $(CCRES) \
                 -I include -I boot/loader -I contrib/lua
BIOS_CFLAGS := -target x86_64-unknown-none-elf -fno-pic -fno-pie -mcmodel=small \
               $(COMMON_CFLAGS) -I boot/bios -DPRISM_BIOS
EFI_CFLAGS  := -target x86_64-unknown-windows -fshort-wchar -mno-stack-arg-probe \
               $(COMMON_CFLAGS) -I boot/efi -DPRISM_EFI
LUA_FLAGS   := -w -include boot/loader/lua_prism.h

LIBSA_SRCS  := $(addprefix boot/libsa/,string.c stdlib.c printf.c math.c stdio.c setjmp.c)
LOADER_SRCS := $(addprefix boot/loader/,boot.c console.c env.c fsutil.c lua_loader.c main.c term.c serial.c)
LUA_SRCS    := $(wildcard contrib/lua/*.c)
BIOS_SRCS   := boot/bios/bios.c boot/bios/disk.c boot/loader/fat.c boot/loader/ext4.c

COMMON_SRCS := $(LIBSA_SRCS) $(LOADER_SRCS) $(LUA_SRCS)

BIOS_OBJS := $(patsubst %.c,$B/bios/%.o,$(COMMON_SRCS) $(BIOS_SRCS)) $B/bios/boot/bios/start.o
EFI_OBJS  := $(patsubst %.c,$B/efi/%.o,$(COMMON_SRCS) boot/loader/ext4.c $(wildcard boot/efi/*.c))

# ---- default target ----------------------------------------------------

.PHONY: all clean run run-uefi run-iso test kernel esp root iso run-kvm run-uefi-kvm data fstest
all: $B/$(NAME).img

# ---- compile rules -------------------------------------------------------

$B/bios/contrib/lua/%.o: contrib/lua/%.c boot/loader/lua_prism.h
	@mkdir -p $(@D)
	$(CC) $(BIOS_CFLAGS) $(LUA_FLAGS) -c -o $@ $<

$B/bios/boot/libsa/%.o: boot/libsa/%.c
	@mkdir -p $(@D)
	$(CC) $(BIOS_CFLAGS) -fno-builtin -c -o $@ $<

$B/bios/%.o: %.c
	@mkdir -p $(@D)
	$(CC) $(BIOS_CFLAGS) -c -o $@ $<

$B/bios/boot/bios/start.o: boot/bios/start.S
	@mkdir -p $(@D)
	$(AS) -c -I include -o $@ $<

$B/efi/contrib/lua/%.o: contrib/lua/%.c boot/loader/lua_prism.h
	@mkdir -p $(@D)
	$(CC) $(EFI_CFLAGS) $(LUA_FLAGS) -c -o $@ $<

$B/efi/boot/libsa/%.o: boot/libsa/%.c
	@mkdir -p $(@D)
	$(CC) $(EFI_CFLAGS) -fno-builtin -c -o $@ $<

$B/efi/%.o: %.c
	@mkdir -p $(@D)
	$(CC) $(EFI_CFLAGS) -c -o $@ $<

# ---- BIOS boot chain ---------------------------------------------------------

$B/boot0.bin: boot/boot0/boot0.S boot/boot0/boot0.ld include/prism.inc
	@mkdir -p $(@D)
	$(AS) -c -x assembler-with-cpp -I include -o $B/boot0.o $<
	$(LD) -m elf_x86_64 -nostdlib -static --no-dynamic-linker -z noexecstack --build-id=none -T boot/boot0/boot0.ld -o $@ $B/boot0.o

$B/boot1.bin: boot/boot1/boot1.S boot/boot1/boot1.ld include/prism.inc
	@mkdir -p $(@D)
	$(AS) -c -x assembler-with-cpp -I include -o $B/boot1.o $<
	$(LD) -m elf_x86_64 -nostdlib -static --no-dynamic-linker -z noexecstack --build-id=none -T boot/boot1/boot1.ld -o $@ $B/boot1.o

$B/loader.bin: $(BIOS_OBJS) boot/bios/loader.ld
	$(LD) -m elf_x86_64 -nostdlib -static --no-dynamic-linker -z noexecstack --build-id=none -T boot/bios/loader.ld -o $@ $(BIOS_OBJS)

# ---- UEFI loader -----------------------------------------------------------

$B/loader.efi: $(EFI_OBJS)
	$(LLD_LINK) /subsystem:efi_application /entry:efi_main /nodefaultlib /machine:x64 \
                /base:0x400000 /dynamicbase /opt:ref /out:$@ $(EFI_OBJS)

# ---- kernel ------------------------------------------------------------------

KERNEL_ELF := sys/kern/target/x86_64-unknown-none/release/kernel

kernel: $B/kernel
$B/kernel: FORCE
	@mkdir -p $(@D)
	cd sys/kern && MAKEFLAGS= $(CARGO) build --release $(CARGOFLAGS)
	-$(PYTHON) tools/mksyms.py $(KERNEL_ELF) sys/kern/src/lib/kallsyms_data.rs
	cd sys/kern && MAKEFLAGS= $(CARGO) build --release $(CARGOFLAGS)
	@cmp -s $(KERNEL_ELF) $@ || cp -f $(KERNEL_ELF) $@
FORCE:

# ---- P1 (ESP) and P3 (ext4 root) trees -------------------------------------------

# P1: minimal FAT ESP - only the UEFI stub the firmware launches.
esp: $B/esp.stamp
$B/esp.stamp: $B/loader.efi
	rm -rf $B/esp
	mkdir -p $B/esp/EFI/BOOT
	cp $B/loader.efi $B/esp/EFI/BOOT/BOOTX64.EFI
	touch $@

# P3: ext4 root, read by the loader (boot tree) and the kernel (root fs).
root: $B/root.stamp
$B/root.stamp: $B/kernel $(wildcard boot/lua/*.lua) $(wildcard boot/conf/*)
	rm -rf $B/root
	mkdir -p $B/root/boot/lua $B/root/boot/defaults $B/root/boot/kernel \
                 $B/root/bin $B/root/etc $B/root/usr
	cp boot/lua/*.lua $B/root/boot/lua/
	cp boot/conf/defaults-loader.conf $B/root/boot/defaults/loader.conf
	cp boot/conf/loader.conf $B/root/boot/loader.conf
	cp $B/kernel $B/root/boot/kernel/kernel
	touch $@

$B/$(NAME).img: $B/boot0.bin $B/boot1.bin $B/loader.bin $B/esp.stamp $B/root.stamp tools/mkimage.py include/prism.inc
	$(PYTHON) tools/mkimage.py --boot0 $B/boot0.bin --boot1 $B/boot1.bin \
                --loader $B/loader.bin --esp-dir $B/esp --root-dir $B/root -o $@

# ---- ext4 data disk (AHCI port 1) -----------------------------------------------
# Created once and kept between runs (so what you write in the shell survives);
# `make clean` or `rm build/data.img` starts over.  The kernel root is P3 of
# the boot disk; this one is mounted from the shell (mount disk1 /mnt).

data: $(DATA_IMG)
$(DATA_IMG):
	@mkdir -p $(@D) $(@D)/datatree
	printf 'Hello from the PrismOS ext4 data disk.\n' > $(@D)/datatree/hello.txt
	truncate -s $(DATA_SIZE) $@
	mke2fs -q -F -t ext4 -L PRISMDATA -d $(@D)/datatree $@

# ---- run / test -----------------------------------------------------------------

run: $B/$(NAME).img $(DATA_IMG)
	$(QEMU) $(QEMUFLAGS) $(call qemu_boot_drive,$<) -serial stdio

run-uefi: $B/$(NAME).img $(DATA_IMG)
	@test -n "$(OVMF_CODE)" || { echo "OVMF not found: set OVMF_CODE and OVMF_VARS"; exit 1; }
	cp -f $(OVMF_VARS) $B/OVMF_VARS.fd
	$(QEMU) $(QEMUFLAGS) -drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
                -drive if=pflash,format=raw,file=$B/OVMF_VARS.fd \
                $(call qemu_boot_drive,$<) -serial stdio

# KVM : BIOS
run-kvm: $B/$(NAME).img $(DATA_IMG)
	@test -e /dev/kvm || { echo "KVM unavailable: /dev/kvm not found"; exit 1; }
	$(QEMU) -enable-kvm $(QEMUFLAGS) \
                $(call qemu_boot_drive,$<) \
                -serial stdio

# KVM : UEFI
run-uefi-kvm: $B/$(NAME).img $(DATA_IMG)
	@test -e /dev/kvm || { echo "KVM unavailable: /dev/kvm not found"; exit 1; }
	@test -n "$(OVMF_CODE)" || { echo "OVMF not found: set OVMF_CODE and OVMF_VARS"; exit 1; }
	@test -f "$(OVMF_CODE)" || { echo "OVMF_CODE file not found: $(OVMF_CODE)"; exit 1; }
	@test -f "$(OVMF_VARS)" || { echo "OVMF_VARS file not found: $(OVMF_VARS)"; exit 1; }
	cp -f $(OVMF_VARS) $B/OVMF_VARS_KVM.fd
	$(QEMU) -enable-kvm $(QEMUFLAGS) \
                -drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
                -drive if=pflash,format=raw,file=$B/OVMF_VARS_KVM.fd \
                $(call qemu_boot_drive,$<) \
                -serial stdio

# ---- ISO --------------------------------------------------------------------

iso: $B/$(NAME).iso

# The ISO keeps the legacy single-volume layout: one FAT image carrying the
# UEFI stub and the whole boot tree (the firmware reads FAT, and the loader's
# EFI simple-file-system fallback mounts it).
$B/isoesp.stamp: $B/loader.efi $B/kernel $(wildcard boot/lua/*.lua) $(wildcard boot/conf/*)
	rm -rf $B/iso-esp
	mkdir -p $B/iso-esp/EFI/BOOT $B/iso-esp/boot/lua $B/iso-esp/boot/defaults $B/iso-esp/boot/kernel
	cp $B/loader.efi $B/iso-esp/EFI/BOOT/BOOTX64.EFI
	cp boot/lua/*.lua $B/iso-esp/boot/lua/
	cp boot/conf/defaults-loader.conf $B/iso-esp/boot/defaults/loader.conf
	cp boot/conf/loader.conf $B/iso-esp/boot/loader.conf
	cp $B/kernel $B/iso-esp/boot/kernel/kernel
	touch $@

$B/esp.img: $B/isoesp.stamp
	truncate -s $(ESPIMG_SIZE) $@
	$(MKFS_FAT) -n PRISMESP $@
	$(MCOPY) -s -i $@ $B/iso-esp/* ::/

$B/$(NAME).iso: $B/esp.img
	rm -rf $B/iso
	mkdir -p $B/iso
	cp $B/esp.img $B/iso/esp.img
	$(XORRISO) -as mkisofs -quiet -V $(NAME) -J -joliet-long -rational-rock \
                -e esp.img -no-emul-boot -isohybrid-gpt-basdat \
                -o $@ $B/iso

run-iso: $B/$(NAME).iso $(DATA_IMG)
	@test -n "$(OVMF_CODE)" || { echo "OVMF not found: set OVMF_CODE and OVMF_VARS"; exit 1; }
	cp -f $(OVMF_VARS) $B/OVMF_VARS.fd
	$(QEMU) $(QEMUFLAGS) -drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
                -drive if=pflash,format=raw,file=$B/OVMF_VARS.fd \
                -cdrom $< -serial stdio

test: $B/$(NAME).img
	$(PYTHON) tests/boot_test.py $< "$(QEMU)" "$(OVMF_CODE)" "$(OVMF_VARS)"

# VFS + ext4 against real images, checked with e2fsck (no QEMU needed).
fstest:
	$(PYTHON) tests/fs_test.py

clean:
	rm -rf $B sys/kern/target
