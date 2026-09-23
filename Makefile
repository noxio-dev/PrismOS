#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# PrismOS -- top-level Makefile
#

NAME    := prismos
CC      := gcc
AS      := gcc
LD      := ld
QEMU    := qemu-system-i386

INCLUDES := -I include/
ASFLAGS  := -m32
LDFLAGS  := -m elf_i386

BOOT0_SRCS  := boot/boot0.S
STAGE2_SRCS := $(wildcard boot/stage2/*.S)

BOOT0_OBJS  := $(patsubst %.S,build/%.o,$(BOOT0_SRCS))
STAGE2_OBJS := $(patsubst %.S,build/%.o,$(STAGE2_SRCS))

IMAGE := build/$(NAME).img

build/%.o: %.S
	@mkdir -p $(@D)
	$(AS) $(INCLUDES) $(ASFLAGS) -c -o $@ $<

build/boot0.bin: $(BOOT0_OBJS) boot/boot0.ld
	@mkdir -p $(@D)
	$(LD) $(LDFLAGS) -T boot/boot0.ld -o $@ $(BOOT0_OBJS)
	@sz=$$(stat -c%s $@); \
	if [ "$$sz" -ne 512 ]; then \
		echo "ERREUR: boot0.bin fait $$sz octets (512 attendus) -- .text trop gros ?"; \
		rm -f $@; exit 1; \
	fi

ifneq ($(strip $(STAGE2_OBJS)),)
build/stage2.bin: $(STAGE2_OBJS) boot/stage2/stage2.ld
	@mkdir -p $(@D)
	$(LD) $(LDFLAGS) -T boot/stage2/stage2.ld -o $@ $(STAGE2_OBJS)
endif

$(IMAGE): build/boot0.bin $(if $(STAGE2_OBJS),build/stage2.bin)
	@mkdir -p $(@D)
	cp -f build/boot0.bin $(IMAGE)
ifneq ($(strip $(STAGE2_OBJS)),)
	dd if=build/stage2.bin of=$(IMAGE) bs=512 seek=1 conv=notrunc status=none
endif
	truncate -s 1440K $(IMAGE)

.PHONY: all clean run

all: $(IMAGE)

run: $(IMAGE)
	$(QEMU) -enable-kvm -drive file=$(IMAGE),format=raw -no-reboot

clean:
	rm -rf build