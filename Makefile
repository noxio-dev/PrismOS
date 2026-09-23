#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#

NAME    := prismos
CC      := gcc
AS      := gcc
LD      := ld
QEMU    := qemu-system-i386

INCLUDES := -I include/
ASFLAGS  := -m32
LDFLAGS  := -m elf_i386
CFLAGS   := -m32 -ffreestanding -fno-pic -fno-stack-protector -nostdlib \
            -Wall -Wextra -O2 $(INCLUDES)

BOOT0_SRCS   := boot/boot0.S
LOADER_SRCS  := boot/loader.S

KERN_ASM_SRCS := sys/kern/locore.S
KERN_C_SRCS   := sys/kern/main.c

BOOT0_OBJS  := $(patsubst %.S,build/%.o,$(BOOT0_SRCS))
LOADER_OBJS := $(patsubst %.S,build/%.o,$(LOADER_SRCS))

KERN_ASM_OBJS := $(patsubst %.S,build/%.o,$(KERN_ASM_SRCS))
KERN_C_OBJS   := $(patsubst %.c,build/%.o,$(KERN_C_SRCS))
KERN_OBJS     := $(KERN_ASM_OBJS) $(KERN_C_OBJS)

IMAGE := build/$(NAME).img

build/%.o: %.S
	@mkdir -p $(@D)
	$(AS) $(INCLUDES) $(ASFLAGS) -c -o $@ $<

build/%.o: %.c
	@mkdir -p $(@D)
	$(CC) $(CFLAGS) -c -o $@ $<

build/boot0.bin: $(BOOT0_OBJS) boot/boot0.ld
	@mkdir -p $(@D)
	$(LD) $(LDFLAGS) -T boot/boot0.ld -o $@ $(BOOT0_OBJS)
	@sz=$$(stat -c%s $@); \
	if [ "$$sz" -ne 512 ]; then \
		echo "ERREUR: boot0.bin fait $$sz octets (512 attendus)"; \
		rm -f $@; exit 1; \
	fi

build/loader.bin: $(LOADER_OBJS) boot/loader.ld
	@mkdir -p $(@D)
	$(LD) $(LDFLAGS) -T boot/loader.ld -o $@ $(LOADER_OBJS)
	@max=$$(( 8 * 512 )); \
	sz=$$(stat -c%s $@); \
	if [ "$$sz" -gt "$$max" ]; then \
		echo "ERREUR: loader.bin fait $$sz octets (max $$max, voir LOADER_SECTORS)"; \
		rm -f $@; exit 1; \
	fi

build/kernel.bin: $(KERN_OBJS) sys/kern/kernel.ld
	@mkdir -p $(@D)
	$(LD) $(LDFLAGS) -T sys/kern/kernel.ld -o $@ $(KERN_OBJS)
	@max=$$(( 16 * 512 )); \
	sz=$$(stat -c%s $@); \
	if [ "$$sz" -gt "$$max" ]; then \
		echo "ERREUR: kernel.bin fait $$sz octets (max $$max, voir KERNEL_SECTORS)"; \
		rm -f $@; exit 1; \
	fi

$(IMAGE): build/boot0.bin build/loader.bin build/kernel.bin
	@mkdir -p $(@D)
	cp -f build/boot0.bin $(IMAGE)
	dd if=build/loader.bin of=$(IMAGE) bs=512 seek=1 conv=notrunc status=none
	dd if=build/kernel.bin of=$(IMAGE) bs=512 seek=9 conv=notrunc status=none
	truncate -s 1440K $(IMAGE)

.PHONY: all clean run

all: $(IMAGE)

run: $(IMAGE)
	$(QEMU) -enable-kvm -drive file=$(IMAGE),format=raw -no-reboot

clean:
	rm -rf build
