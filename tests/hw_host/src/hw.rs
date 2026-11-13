// The kernel driver sources, included unchanged at their kernel module paths
// (crate::hw::nvme ...), plus small shims for what they reach into.
#[path = "../../../sys/kern/src/hw/blk.rs"]
pub mod blk;
#[path = "../../../sys/kern/src/hw/hal.rs"]
pub mod hal;
#[path = "../../../sys/kern/src/hw/disk.rs"]
pub mod disk;
#[path = "../../../sys/kern/src/hw/nvme.rs"]
pub mod nvme;
#[path = "../../../sys/kern/src/hw/xhci.rs"]
pub mod xhci;
#[path = "../../../sys/kern/src/hw/usbhid.rs"]
pub mod usbhid;
#[path = "../../../sys/kern/src/hw/usbmsc.rs"]
pub mod usbmsc;

pub mod kbd {
    use std::sync::Mutex;
    pub static LOG: Mutex<Vec<(u8, bool, bool)>> = Mutex::new(Vec::new());
    pub fn inject_key(sc: u8, ext: bool, release: bool) {
        LOG.lock().unwrap().push((sc, ext, release));
    }
}

pub mod ahci {
    use crate::hw::blk::{BlkError, BlockDev};
    pub const MAX_DISKS: usize = 4;
    #[derive(Clone, Copy)]
    pub struct AhciDisk;
    pub fn disk(_i: usize) -> Option<AhciDisk> {
        None
    }
    impl AhciDisk {
        pub fn model(&self) -> &'static str { "" }
        pub fn serial(&self) -> &'static str { "" }
    }
    impl BlockDev for AhciDisk {
        fn sectors(&self) -> u64 { 0 }
        fn read(&self, _: u64, _: &mut [u8]) -> Result<(), BlkError> { Err(BlkError::NoDevice) }
        fn write(&self, _: u64, _: &[u8]) -> Result<(), BlkError> { Err(BlkError::NoDevice) }
        fn flush(&self) -> Result<(), BlkError> { Ok(()) }
    }
}

pub mod pci {
    use std::sync::Mutex;
    pub const CMD_MEM: u16 = 2;
    pub const CMD_BUS_MASTER: u16 = 4;
    pub const CMD_INTX_DISABLE: u16 = 1 << 10;
    #[derive(Clone, Copy)]
    pub struct Bar { pub addr: u64, pub size: u64, pub io: bool, pub prefetch: bool, pub is64: bool }
    #[derive(Clone, Copy)]
    pub struct PciDev {
        pub bus: u8, pub dev: u8, pub func: u8, pub vendor: u16, pub device: u16,
        pub class: u8, pub subclass: u8, pub progif: u8, pub bar0: Bar,
    }
    pub static DEVS: Mutex<Vec<PciDev>> = Mutex::new(Vec::new());
    pub static ENABLED: Mutex<u16> = Mutex::new(0);
    pub fn devices() -> &'static [PciDev] {
        let v: Vec<PciDev> = DEVS.lock().unwrap().clone();
        Box::leak(v.into_boxed_slice())
    }
    impl PciDev {
        pub fn bar(&self, i: usize) -> Option<Bar> { if i == 0 { Some(self.bar0) } else { None } }
        pub fn enable(&self, bits: u16) { *ENABLED.lock().unwrap() |= bits; }
    }
}
