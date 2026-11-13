fn main() {
    // The kernel's hal.rs swaps its register / DMA / clock primitives for
    // `hostsim_*` hooks under this cfg (see sys/kern/src/hw/hal.rs).
    println!("cargo:rustc-cfg=hostsim");
    println!("cargo:rustc-check-cfg=cfg(hostsim)");
}
