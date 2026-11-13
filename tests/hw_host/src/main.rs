#![allow(dead_code, unused_imports, unexpected_cfgs, static_mut_refs)]
extern crate alloc;

#[macro_export]
macro_rules! kprintln { ($($t:tt)*) => { println!($($t)*) } }
#[macro_export]
macro_rules! vprintln { ($($t:tt)*) => { println!($($t)*) } }

#[path = "lib_mod.rs"]
mod lib;
mod hw;
mod models;
mod xmodel;
mod scenarios;

fn main() {
    let which = std::env::args().nth(1).unwrap_or_default();
    match which.as_str() {
        "nvme512" => scenarios::nvme(512),
        "nvme4k" => scenarios::nvme(4096),
        "usb32" => scenarios::usb(false, false),
        "usb64" => scenarios::usb(true, true),
        _ => {
            eprintln!("usage: hw_host nvme512|nvme4k|usb32|usb64");
            std::process::exit(2);
        }
    }
    println!("PASS {}", which);
}
