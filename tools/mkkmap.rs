// SPDX-License-Identifier: GPL-3.0-or-later
//
// Writes the built-in keymaps as BusyBox `loadkmap` files:
//
//   rustc --edition 2021 tools/mkkmap.rs -o /tmp/mkkmap && /tmp/mkkmap rootfs/usr/share/keymaps
//
// -> us.bkmap, fr.bkmap (same format as `loadkeys --bkeymap`).

#[path = "../sys/kern/src/hw/keymap.rs"]
mod keymap;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: mkkmap <output dir>");
    for l in [&keymap::LAYOUT_US, &keymap::LAYOUT_FR] {
        let mut km = keymap::Keymap::new();
        keymap::build_default(&mut km, l);
        let mut buf = vec![0u8; 7 + keymap::MAX_MAPS + 2 * keymap::NR_KEYS * keymap::MAX_MAPS];
        let n = km.to_bkmap(&mut buf);
        std::fs::write(format!("{}/{}.bkmap", dir, l.name), &buf[..n]).unwrap();
    }
}
