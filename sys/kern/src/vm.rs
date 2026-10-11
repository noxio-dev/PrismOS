// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev

use crate::cpu::cpuid;
use core::str::from_utf8;

const HYPERVISOR_BIT: u32 = 1 << 31;

const HYPERVISOR_LEAF: u32 = 0x4000_0000;

const KVM_FEATURES_LEAF: u32 = 0x4000_0001;

const SIGNATURES: &[(&[u8; 12], Hypervisor)] = &[
    (b"KVMKVMKVM\0\0\0", Hypervisor::Kvm),
    (b"Microsoft Hv", Hypervisor::HyperV),
    (b"VMwareVMware", Hypervisor::Vmware),
    (b"VBoxVBoxVBox", Hypervisor::VirtualBox),
    (b"XenVMMXenVMM", Hypervisor::Xen),
    (b"prl hyperv\0\0", Hypervisor::Parallels),
    (b"lrpepyh vr\0\0", Hypervisor::Parallels),
    (b"bhyve bhyve\0", Hypervisor::Bhyve),
    (b"TCGTCGTCGTCG", Hypervisor::QemuTcg),
    (b"QNXQVMBSQG\0\0", Hypervisor::Qnx),
    (b"ACRNACRNACRN", Hypervisor::Acrn),
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hypervisor {
    Kvm,
    HyperV,
    Vmware,
    VirtualBox,
    Xen,
    Parallels,
    Bhyve,
    QemuTcg,
    Qnx,
    Acrn,
    Unknown,
}

impl Hypervisor {
    pub fn name(self) -> &'static str {
        match self {
            Hypervisor::Kvm => "KVM",
            Hypervisor::HyperV => "Microsoft Hyper-V",
            Hypervisor::Vmware => "VMware",
            Hypervisor::VirtualBox => "Oracle VirtualBox",
            Hypervisor::Xen => "Xen",
            Hypervisor::Parallels => "Parallels",
            Hypervisor::Bhyve => "bhyve",
            Hypervisor::QemuTcg => "QEMU (TCG, émulation logicielle)",
            Hypervisor::Qnx => "QNX hypervisor",
            Hypervisor::Acrn => "ACRN",
            Hypervisor::Unknown => "hyperviseur inconnu",
        }
    }
}

pub struct VmInfo {
    pub hypervisor: Hypervisor,
    pub signature: [u8; 12],
    pub max_leaf: u32,
}

impl VmInfo {
    pub fn name(&self) -> &'static str {
        self.hypervisor.name()
    }

    pub fn signature_str(&self) -> &str {
        let end = self.signature.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
        from_utf8(&self.signature[..end]).unwrap_or("<invalid>")
    }

    pub fn kvm_features(&self) -> Option<u32> {
        if self.hypervisor != Hypervisor::Kvm || self.max_leaf < KVM_FEATURES_LEAF {
            return None;
        }
        Some(cpuid(KVM_FEATURES_LEAF, 0).eax)
    }
}

pub fn detect() -> Option<VmInfo> {
    if cpuid(1, 0).ecx & HYPERVISOR_BIT == 0 {
        return None;
    }

    let sig = cpuid(HYPERVISOR_LEAF, 0);
    let mut bytes = [0u8; 12];
    bytes[0..4].copy_from_slice(&sig.ebx.to_le_bytes());
    bytes[4..8].copy_from_slice(&sig.ecx.to_le_bytes());
    bytes[8..12].copy_from_slice(&sig.edx.to_le_bytes());

    let hypervisor = SIGNATURES
        .iter()
        .find(|(s, _)| *s == &bytes)
        .map(|(_, h)| *h)
        .unwrap_or(Hypervisor::Unknown);

    Some(VmInfo { hypervisor, signature: bytes, max_leaf: sig.eax })
}