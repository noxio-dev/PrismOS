// SPDX-License-Identifier: GPL-3.0-or-later
//
// The kernel's identity, in one place: what `uname(2)` answers and what
// /proc/version prints.  Nothing here is typed by hand twice -- the release
// is the crate version, the build number and date come from build.rs, so a
// new build is a new `uname -v` and the userland tests check the two
// surfaces against each other instead of against a literal.
//
//     $ uname -a
//     PrismOS prismos 0.3.4 #82 Sun, 08 Nov 2026 17:22:00 +0000 x86_64 PrismOS
//
// The node name starts as "prismos" and the `hostname=` option of the
// kernel command line replaces it (loader.conf: kernel_options).

use alloc::string::String;

use crate::lib::spin::SpinLock;

pub const SYSNAME: &str = "PrismOS";
pub const RELEASE: &str = env!("CARGO_PKG_VERSION");
pub const MACHINE: &str = "x86_64";
pub const DOMAINNAME: &str = "(none)";
pub const DEFAULT_NODENAME: &str = "prismos";

/// "#<build number> <RFC 2822 build date>" -- the utsname version field.
pub const VERSION: &str = concat!(
    "#",
    env!("PRISMOS_BUILD_NUMBER"),
    " ",
    env!("PRISMOS_BUILD_DATE")
);

/// The user the kernel reports as its builder in /proc/version.
pub const BUILDER: &str = "noxiio-dev";

static NODENAME: SpinLock<Option<String>> = SpinLock::new(None);

/// The node name (hostname) right now.
pub fn nodename() -> String {
    match &*NODENAME.lock() {
        Some(n) => n.clone(),
        None => String::from(DEFAULT_NODENAME),
    }
}

/// Replace the node name; an empty or over-long one (utsname fields are 64
/// bytes plus the NUL) is refused and false comes back.
pub fn set_nodename(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 || name.bytes().any(|b| b == 0 || b == b'\n') {
        return false;
    }
    *NODENAME.lock() = Some(String::from(name));
    true
}

/// Read `hostname=<name>` out of the kernel command line, if it is there.
pub fn apply_cmdline(cmdline: &str) {
    for opt in cmdline.split_whitespace() {
        if let Some(v) = opt.strip_prefix("hostname=") {
            set_nodename(v);
        }
    }
}

/// The /proc/version line, Linux's shape.
pub fn proc_version() -> String {
    alloc::format!(
        "{} version {} ({}) {}\n",
        SYSNAME,
        RELEASE,
        BUILDER,
        VERSION
    )
}
