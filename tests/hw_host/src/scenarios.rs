use crate::hw::blk::BlockDev;
use crate::hw::{disk, nvme, pci};
use crate::models::*;
use crate::xmodel::*;
use std::sync::{Arc, Mutex};

/// Deterministic pseudo-random bytes.
fn pat(seed: u32, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2654435761).wrapping_add(1);
    (0..n).map(|_| { x ^= x << 13; x ^= x >> 17; x ^= x << 5; x as u8 }).collect()
}

/// Write / read patterns through the 512-byte BlockDev view and compare with
/// the model's own bytes.
fn exercise(d: &disk::Disk, backing: &dyn Fn() -> Vec<u8>) {
    let total = d.sectors();
    assert!(total >= 1024);
    let cases: [(u64, usize); 9] = [(0, 1), (1, 1), (7, 3), (8, 8), (13, 1), (100, 256), (300, 7), (512, 512 + 3), (total - 5, 5)];
    for (k, &(lba, n)) in cases.iter().enumerate() {
        let data = pat(k as u32 + 1, n * 512);
        d.write(lba, &data).unwrap_or_else(|e| panic!("write lba {} n {}: {:?}", lba, n, e));
        let mut back = vec![0u8; n * 512];
        d.read(lba, &mut back).unwrap_or_else(|e| panic!("read lba {} n {}: {:?}", lba, n, e));
        assert!(back == data, "readback mismatch lba {} n {}", lba, n);
        let img = backing();
        assert!(img[lba as usize * 512..(lba as usize + n) * 512] == data[..], "model disk differs lba {} n {}", lba, n);
    }
    // Neighbours of a one-sector write must survive (read-modify-write).
    let before = backing();
    let one = pat(99, 512);
    d.write(21, &one).unwrap();
    let after = backing();
    assert!(after[..21 * 512] == before[..21 * 512] && after[22 * 512..] == before[22 * 512..], "RMW damaged neighbours");
    assert!(after[21 * 512..22 * 512] == one[..]);
    // Out of range and flush.
    let mut b = vec![0u8; 512];
    assert!(d.read(total, &mut b).is_err());
    d.flush().unwrap();
}

pub fn nvme(bs: usize) {
    let blocks = 4 * 1024 * 1024 / bs;
    let m = Arc::new(Mutex::new(Nvme::new(bs, blocks, 1)));
    let bar = 0xFE00_0000u64;
    add_region(bar, 0x4000, Box::new(NvmeHandle(m.clone())));
    pci::DEVS.lock().unwrap().push(pci::PciDev {
        bus: 0, dev: 4, func: 0, vendor: 0x1b36, device: 0x10, class: 1, subclass: 8, progif: 2,
        bar0: pci::Bar { addr: bar, size: 0x4000, io: false, prefetch: false, is64: true },
    });
    let n = nvme::init();
    assert_eq!(n, 1, "one namespace expected");
    disk::rescan();
    assert_eq!(disk::count(), 1);
    let d = disk::get(0).unwrap();
    assert_eq!(d.bus(), disk::Bus::Nvme);
    println!("nvme: \"{}\" serial \"{}\" block {} sectors {}", d.model(), d.serial(), d.block_size(), d.sectors());
    assert_eq!(d.block_size(), bs);
    assert_eq!(d.sectors(), (4 * 1024 * 1024 / 512) as u64);
    assert!(d.model().starts_with("PRISM VIRTUAL NVME"));
    let mm = m.clone();
    exercise(&d, &move || mm.lock().unwrap().disk.clone());
    let g = m.lock().unwrap();
    println!("nvme: {} I/O commands, largest {} blocks", g.io_cmds, g.max_blocks_seen);
}


fn msc_bytes(x: &Arc<Mutex<Xhci>>, port: usize) -> Vec<u8> {
    match x.lock().unwrap().dev(port) { Dev::Msc(m) => m.disk.clone(), _ => panic!() }
}

pub fn usb(csz64: bool, double: bool) {
    use crate::hw::{hal, usbhid, xhci, kbd};
    let mut x = Xhci::new(csz64);
    x.double_event = double;
    x.plug(1, 1, Dev::Kbd(Kbd { reports: Default::default(), stall_set_idle: true, protocol_set: None }));
    x.plug(2, 3, Dev::Msc(Msc::new(512, 4 << 20)));
    x.plug(3, 4, Dev::Msc(Msc::new(4096, 4 << 20)));
    let x = Arc::new(Mutex::new(x));
    let bar = 0xFD00_0000u64;
    add_region(bar, 0x1000, Box::new(XHandle(x.clone())));
    pci::DEVS.lock().unwrap().push(pci::PciDev {
        bus: 0, dev: 5, func: 0, vendor: 0x1b36, device: 0x0d, class: 0x0C, subclass: 3, progif: 0x30,
        bar0: pci::Bar { addr: bar, size: 0x1000, io: false, prefetch: false, is64: true },
    });
    assert_eq!(xhci::init(), 1);
    disk::rescan();
    let list = xhci::list();
    for d in &list { println!("usb: slot {} port {} {} {:04x}:{:04x} {}", d.h.slot, d.port, d.speed, d.vid, d.pid, d.class); }
    assert_eq!(list.len(), 3);
    assert!(x.lock().unwrap().saw_eval_ctx, "full-speed EP0 packet size was not corrected");
    assert_eq!(disk::count(), 2);
    let (a, b) = (disk::get(0).unwrap(), disk::get(1).unwrap());
    assert_eq!((a.bus(), b.bus()), (disk::Bus::Usb, disk::Bus::Usb));
    println!("usb: disk0 \"{}\" bs {} sectors {}; disk1 \"{}\" bs {} sectors {}", a.model(), a.block_size(), a.sectors(), b.model(), b.block_size(), b.sectors());
    assert_eq!((a.block_size(), b.block_size()), (512, 4096));
    assert_eq!(a.model(), "PRISMOS USB TEST DISK");

    // Storage.
    let xa = x.clone();
    exercise(&a, &move || msc_bytes(&xa, 2));
    let xb = x.clone();
    exercise(&b, &move || msc_bytes(&xb, 3));

    // A data-in stall in the middle of a read: cleared, retried, data right.
    if let Dev::Msc(m) = x.lock().unwrap().dev(2) { m.stall_next_data_in = true; }
    let mut buf = vec![0u8; 8 * 512];
    a.read(40, &mut buf).expect("read after a stall");
    assert!(buf[..] == msc_bytes(&x, 2)[40 * 512..48 * 512]);
    if let Dev::Msc(m) = x.lock().unwrap().dev(2) { assert_eq!(m.stalls, 1); }
    println!("usb: stall recovery ok");

    // Keyboard.
    let log = || { let mut l = kbd::LOG.lock().unwrap(); let v = l.clone(); l.clear(); v };
    log();
    let rep = |mods: u8, k: [u8; 6]| { let mut r = [0u8; 8]; r[0] = mods; r[2..].copy_from_slice(&k); r };
    x.lock().unwrap().kbd_report(1, rep(0, [4, 0, 0, 0, 0, 0]));
    xhci::poll();
    assert_eq!(log(), vec![(0x1E, false, false)]);
    x.lock().unwrap().kbd_report(1, rep(0, [0; 6]));
    xhci::poll();
    assert_eq!(log(), vec![(0x1E, false, true)]);
    // Shift + a, then Up arrow (extended) while a is still down.
    x.lock().unwrap().kbd_report(1, rep(2, [4, 0, 0, 0, 0, 0]));
    xhci::poll();
    assert_eq!(log(), vec![(0x2A, false, false), (0x1E, false, false)]);
    x.lock().unwrap().kbd_report(1, rep(2, [4, 0x52, 0, 0, 0, 0]));
    xhci::poll();
    assert_eq!(log(), vec![(0x48, true, false)]);
    x.lock().unwrap().kbd_report(1, rep(0, [0; 6]));
    xhci::poll();
    let l = log();
    assert!(l.contains(&(0x2A, false, true)) && l.contains(&(0x1E, false, true)) && l.contains(&(0x48, true, true)), "{:?}", l);
    // Rollover error report is ignored.
    x.lock().unwrap().kbd_report(1, rep(0, [1; 6]));
    xhci::poll();
    assert!(log().is_empty());
    // Auto-repeat of a held key.
    x.lock().unwrap().kbd_report(1, rep(0, [5, 0, 0, 0, 0, 0]));
    for _ in 0..60 { xhci::poll(); }
    let l = log();
    let presses = l.iter().filter(|e| **e == (0x30, false, false)).count();
    assert!(presses >= 4, "expected repeats, got {} presses", presses);
    // Unplug with the key held: it is released.
    x.lock().unwrap().unplug(1);
    xhci::rescan();
    assert!(log().contains(&(0x30, false, true)));
    println!("usb: keyboard ok");

    // Hot unplug / replug of a storage device.
    x.lock().unwrap().unplug(2);
    assert_eq!(xhci::rescan(), 0);
    disk::rescan();
    assert_eq!(disk::count(), 1);
    assert_eq!(disk::get(0).unwrap().block_size(), 4096);
    assert_eq!(xhci::list().len(), 1);
    x.lock().unwrap().plug(2, 3, Dev::Msc(Msc::new(512, 2 << 20)));
    assert_eq!(xhci::rescan(), 1);
    disk::rescan();
    assert_eq!(disk::count(), 2);
    let n = disk::get(0).unwrap(); // the freed LUN slot is reused
    let xn = x.clone();
    assert_eq!(n.sectors(), 4096);
    exercise_small(&n, &move || msc_bytes(&xn, 2));
    println!("usb: hotplug ok; commands seen: {:?}", x.lock().unwrap().cmds_seen.len());
}

fn exercise_small(d: &disk::Disk, backing: &dyn Fn() -> Vec<u8>) {
    let data = pat(7, 3 * 512);
    d.write(5, &data).unwrap();
    assert!(backing()[5 * 512..8 * 512] == data[..]);
}
