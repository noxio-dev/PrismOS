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

    // The utsname "version" field, Linux's shape: "#<build> <RFC 2822 date>"
    // (what `uname -v` prints).  The build number is the commit count when
    // the tree is a git checkout, 1 otherwise; the date is the build's.
    println!("cargo:rustc-env=PRISMOS_BUILD_NUMBER={}", build_number());
    println!("cargo:rustc-env=PRISMOS_BUILD_DATE={}", rfc2822(ts));
}

/// Commit count of the checkout (`git rev-list --count HEAD`), or 1 when
/// there is no git or no repository (a source tarball).
fn build_number() -> u64 {
    std::process::Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1)
}

/// "Sun, 08 Nov 2026 17:22:00 +0000" from a unix timestamp -- the date
/// format Linux puts in its utsname version.
fn rfc2822(ts: u64) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // build_date() gives "YYYY-MM-DD HH:MM:SS UTC"; reuse it rather than
    // redo the calendar maths.
    let d = build_date(ts);
    let (y, mo, day) = (&d[0..4], d[5..7].parse::<usize>().unwrap(), &d[8..10]);
    let time = &d[11..19];
    // 1970-01-01 was a Thursday.
    let wd = DAYS[((ts / 86400) % 7) as usize];
    format!("{wd}, {day} {} {y} {time} +0000", MONTHS[mo - 1])
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