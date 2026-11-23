# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
"""Pieces shared by the QEMU-driven tests (boot_test, shell_drive, ...)."""

import os

# /etc/keyboard-setup (run by /etc/rc) blocks on this menu until it gets a
# number.  Every test that boots the userland session has to answer it.
KBD_PROMPT = "Select layout [1-5]:"
# US (2): the scripted keystrokes keep their QWERTY meaning.
KBD_ANSWER = b"2\r"


def kvm_usable():
    """True when QEMU can use /dev/kvm.

    PRISM_KVM=0 forces software emulation (TCG), PRISM_KVM=1 asks for KVM
    (still falling back to TCG, with a note, when /dev/kvm is not usable);
    unset means "KVM when available".
    """
    want = os.environ.get("PRISM_KVM", "").strip().lower()
    if want in ("0", "no", "false", "off"):
        return False
    ok = os.access("/dev/kvm", os.R_OK | os.W_OK)
    if not ok and want in ("1", "yes", "true", "on"):
        print("note: PRISM_KVM=1 but /dev/kvm is not usable, using TCG")
    return ok


def kvm_flags():
    """QEMU arguments that turn KVM on (no -cpu: the default qemu64 model,
    like the Makefile's run-kvm, keeps the CPU brand the tests pin)."""
    return ["-enable-kvm"] if kvm_usable() else []
