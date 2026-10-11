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
QEMUFLAGS ?= -m 256M
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
BIOS_SRCS   := boot/bios/bios.c boot/bios/disk.c boot/loader/fat.c

COMMON_SRCS := $(LIBSA_SRCS) $(LOADER_SRCS) $(LUA_SRCS)

BIOS_OBJS := $(patsubst %.c,$B/bios/%.o,$(COMMON_SRCS) $(BIOS_SRCS)) $B/bios/boot/bios/start.o
EFI_OBJS  := $(patsubst %.c,$B/efi/%.o,$(COMMON_SRCS) $(wildcard boot/efi/*.c))

# ---- default target ----------------------------------------------------

.PHONY: all clean run run-uefi run-iso test kernel esp iso
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
	@cmp -s $(KERNEL_ELF) $@ || cp -f $(KERNEL_ELF) $@
FORCE:

# ---- ESP tree and disk image ---------------------------------------------------

esp: $B/esp.stamp
$B/esp.stamp: $B/loader.efi $B/kernel $(wildcard boot/lua/*.lua) $(wildcard boot/conf/*)
	rm -rf $B/esp
	mkdir -p $B/esp/EFI/BOOT $B/esp/boot/lua $B/esp/boot/defaults $B/esp/boot/kernel
	cp $B/loader.efi $B/esp/EFI/BOOT/BOOTX64.EFI
	cp $B/loader.efi $B/esp/boot/loader.efi
	cp boot/lua/*.lua $B/esp/boot/lua/
	cp boot/conf/defaults-loader.conf $B/esp/boot/defaults/loader.conf
	cp boot/conf/loader.conf $B/esp/boot/loader.conf
	cp $B/kernel $B/esp/boot/kernel/kernel
	touch $@

$B/$(NAME).img: $B/boot0.bin $B/boot1.bin $B/loader.bin $B/esp.stamp tools/mkimage.py include/prism.inc
	$(PYTHON) tools/mkimage.py --boot0 $B/boot0.bin --boot1 $B/boot1.bin \
		--loader $B/loader.bin --esp-dir $B/esp -o $@

# ---- run / test -----------------------------------------------------------------

run: $B/$(NAME).img
	$(QEMU) $(QEMUFLAGS) -drive file=$<,format=raw -serial stdio

run-uefi: $B/$(NAME).img
	@test -n "$(OVMF_CODE)" || { echo "OVMF not found: set OVMF_CODE and OVMF_VARS"; exit 1; }
	cp -f $(OVMF_VARS) $B/OVMF_VARS.fd
	$(QEMU) $(QEMUFLAGS) -drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
		-drive if=pflash,format=raw,file=$B/OVMF_VARS.fd \
		-drive file=$<,format=raw -serial stdio

# ---- ISO --------------------------------------------------------------------

iso: $B/$(NAME).iso

 $B/esp.img: $B/esp.stamp
	truncate -s $(ESPIMG_SIZE) $@
	$(MKFS_FAT) -n PRISMESP $@
	$(MCOPY) -s -i $@ $B/esp/* ::/

 $B/$(NAME).iso: $B/esp.img
	rm -rf $B/iso
	mkdir -p $B/iso
	cp $B/esp.img $B/iso/esp.img
	$(XORRISO) -as mkisofs -quiet -V $(NAME) -J -joliet-long -rational-rock \
		-e esp.img -no-emul-boot -isohybrid-gpt-basdat \
		-o $@ $B/iso

run-iso: $B/$(NAME).iso
	@test -n "$(OVMF_CODE)" || { echo "OVMF not found: set OVMF_CODE and OVMF_VARS"; exit 1; }
	cp -f $(OVMF_VARS) $B/OVMF_VARS.fd
	$(QEMU) $(QEMUFLAGS) -drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
		-drive if=pflash,format=raw,file=$B/OVMF_VARS.fd \
		-cdrom $< -serial stdio
		
test: $B/$(NAME).img
	$(PYTHON) tests/boot_test.py $< "$(QEMU)" "$(OVMF_CODE)" "$(OVMF_VARS)"

clean:
	rm -rf $B sys/kern/target
