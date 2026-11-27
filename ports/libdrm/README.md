# libdrm — step 2 of the Xorg port

```
ports/libdrm/
├── build.sh                           fetch (+ patch) → setup → build →
│                                      install → verify → test
├── prism-cc                           the C compiler of the meson cross file
├── patches/
│   └── libdrm-2.4.124-prismos.patch   the Linux code paths, for __prismos__
├── drmtest/drmtest.c                  the milestone-M3 program
└── README.md
```

Step 2 of `docs/XORG.md` section 3: **libdrm 2.4.124**, core library only
(`xf86drm.c`, `xf86drmMode.c`, ...), no GPU driver — the kernel's
`/dev/dri/card0` is a simpledrm-like device (1 CRTC, 1 primary plane, dumb
buffers, page flip, atomic).  libdrm is meson-only, so this is a meson cross
build against the `x86_64-prismos` toolchain (`make mlibc` first).  It does
not need the packages of step 1.

## Usage

```
sh ports/libdrm/build.sh                # every stage
sh ports/libdrm/build.sh stage setup    # fetch setup build install verify test
make libdrm                             # same thing, with a stamp
LIBDRM_LIBRARY=both make libdrm         # also libdrm.so.2.124.0
```

`LIBDRM_VER` (2.4.124 — the patch is per version), `LIBDRM_MIRROR`
(`dri.freedesktop.org/libdrm`), `--prefix` (toolchain), `--src`, `--sysroot`,
`--jobs`.  A `libdrm-2.4.124.tar.xz` (or `.tar.gz`) already in
`build/xorg-src` is used as it is.  Host requirements: meson ≥ 0.59, ninja,
pkg-config, python3, patch, curl, tar + xz.

The result is installed with `--prefix=/usr` into the same sysroot as
`ports/xorg`, `build/xorg-native/sysroot`: `xf86drm.h`, `xf86drmMode.h`,
`libdrm/*.h`, `usr/lib/libdrm.a`, `libdrm.pc` — found by the next ports the
cross way (`PKG_CONFIG_SYSROOT_DIR=$SYSROOT`,
`PKG_CONFIG_LIBDIR=$SYSROOT/usr/share/pkgconfig:$SYSROOT/usr/lib/pkgconfig`).
Nothing is installed into the image.

## What is in the port, and why

**The patch.**  libdrm finds and classifies DRM nodes through
`/sys/dev/char/<maj>:<min>/device/...` behind `#ifdef __linux__` (17 places
in `xf86drm.c`), and its public headers `xf86drm.h` and `include/drm/drm.h`
pick the Linux ioctl macros and types the same way.  gcc defines
`__prismos__`, not `__linux__`, so an unmodified libdrm lands in the BSD
branches: `<sys/ioccom.h>` (not in mlibc), `devname_r`, and a `drm_handle_t`
that is an `unsigned long` instead of the kernel's `unsigned int`.  The patch
makes those 19 conditions `defined(__linux__) || defined(__prismos__)`.
The kernel side is already there: `sys/kern/src/fs/sysfs.rs` serves
`/sys/dev/char/226:0`, `device/subsystem` (`platform`) and `device/uevent`
for exactly this.  `drmCheckModesettingSupported` is left alone (it reads
`/sys/bus/pci`, which is not served).

**prism-cc.**  `x86_64-prismos-gcc` does not link for PrismOS by itself —
every program of the tree is linked by an explicit `ld` call — and meson
links probes while it configures (`cc.links` for the atomics,
`has_function('clock_gettime')`, the sanity check).  `prism-cc` is the
driver of `ports/nano` reduced to compile / static-PIE link / DSO link.

**Options.**  Every GPU driver off (`intel` is `auto` on x86 and would
insist on libpciaccess), `udev=false`, `tests=false`, `man-pages`,
`cairo-tests`, `valgrind` off.  An option the version's `meson_options.txt`
does not have is skipped instead of handed to meson.  `-Db_lundef=false`
because a DSO leaves libc symbols undefined (ld.so binds them against
`libc.so`).  `buildtype=release`, `--libdir=lib`.  The cross file's
`pkg_config_libdir` points at the sysroot so the optional probes
(`atomic_ops`, `cunit`, `cairo`) never see the build machine's libraries.

**Static by default.**  An archive is the least risky output of the
toolchain's link recipe; `LIBDRM_LIBRARY=both` adds the DSO (SONAME
`libdrm.so.2`) that Xorg's `dlopen()`ed modules will link against, or the
server's `modesetting` can be linked statically against `libdrm.a` (plan B
of `docs/XORG.md` 3.2).

## What `verify` and `test` check

- the headers, `libdrm.a` (and the DSO + its SONAME with `both`) and
  `libdrm.pc` are in the sysroot, and `pkg-config libdrm` says 2.4.124;
- the library defines **every** symbol of the version's `core-symbols.txt`
  (212), the list upstream's own test enforces;
- `drmtest` compiles against the installed headers and links against
  `libdrm.a` + `libc.a` with `prism-cc` — the closure check: every libc
  symbol libdrm uses is answered by mlibc — and is stripped under the
  kernel's 4 MiB whole-file read.

`build/libdrm-out/drmtest` is the program of **milestone M3**
(`drmModeGetResources()` returns the virtual connector): it prints the
driver version, the node type, the resources and each connector, and exits
0 if one is connected.  Copy it into the image and run `drmtest
/dev/dri/card0` under QEMU.

## Status

Not run against the cross toolchain yet.  Checked on the host: the patch
applies to the pristine tarball (and without it `xf86drm.c` stops on the
BSD branch's `<sys/ioccom.h>` when compiled with `__linux__` undefined and
`__prismos__` defined; with it, it compiles); meson accepts the option list;
the patched source configures, builds and installs natively and satisfies
`verify`'s symbol check; `drmtest.c` compiles and runs; `prism-cc`'s three
modes assemble the expected `gcc` / `ld` command lines (against a stub
toolchain).  What the first `make libdrm` will show: whether meson's probes
all link through `prism-cc` (the sanity check, `clock_gettime`, `sys/sysmacros.h`
in mlibc), and whether libdrm's libc needs are all in mlibc (the `test`
stage's link).
