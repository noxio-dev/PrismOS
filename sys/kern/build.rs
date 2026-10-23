// SPDX-License-Identifier: GPL-3.0-or-later
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();

    // Kernel image layout.
    println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");
    println!("cargo:rerun-if-changed=build.rs");

    // IMPORTANT : sans ça, build.rs ne rétourne que quand linker.ld ou
    // build.rs changent, et le stamp serait figé au dernier changement de
    // build.rs -- inutile.  Avec 'src', il rétourne à chaque modif source,
    // donc chaque vraie recompilation reçoit un stamp frais.
    println!("cargo:rerun-if-changed=src");

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=BUILD_TS={ts}");
    println!("cargo:rustc-env=BUILD_DATE={}", build_date(ts));
}

/// YYYY-MM-DD HH:MM UTC from a unix timestamp, no external crates
/// (civil-from-days, Howard Hinnant's algorithm).
fn build_date(ts: u64) -> String {
    let days = (ts / 86400) as i64;
    let rem = ts % 86400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}