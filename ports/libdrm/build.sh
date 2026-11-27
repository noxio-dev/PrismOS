#!/bin/sh
#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# The libdrm port, step 2 of docs/XORG.md section 3: libdrm 2.4.124
# cross-built for x86_64-prismos with no GPU driver -- the core library
# only (xf86drm.c, xf86drmMode.c, ...), which is all the display needs:
# the kernel's /dev/dri/card0 is a simpledrm-like device (1 CRTC, 1
# primary plane, dumb buffers, page flip, atomic).
#
# libdrm is meson-only, so this port is a meson cross build.  The pieces:
#
#   patches/libdrm-2.4.124-prismos.patch
#       the code is full of `#ifdef __linux__` branches -- the ones that
#       find DRM nodes through /sys/dev/char/<maj>:<min> (served by
#       sys/kern/src/fs/sysfs.rs, built for exactly this) and the public
#       headers' Linux ioctl macros and types.  gcc defines __prismos__,
#       not __linux__, so the unknown OS would land in the BSD branches
#       (<sys/ioccom.h>, devname_r, a wrong drm_handle_t).  The patch
#       makes those branches `__linux__ || __prismos__`.
#   prism-cc
#       the C compiler of the cross file: x86_64-prismos-gcc does not link
#       by itself (every program of the tree is linked by an explicit ld
#       call), and meson links probes while it configures.  It is the
#       driver of ports/nano, reduced to compile / static-PIE link / DSO.
#   -Db_lundef=false
#       a DSO leaves libc symbols undefined (ld.so binds them against
#       libc.so), which meson's default --no-undefined would refuse.
#
# The result is installed with --prefix=/usr into the same sysroot as
# ports/xorg (build/xorg-native/sysroot): xf86drm.h, xf86drmMode.h,
# libdrm/*.h, usr/lib/libdrm.a and libdrm.pc, found by the next ports the
# usual cross way (PKG_CONFIG_SYSROOT_DIR + PKG_CONFIG_LIBDIR).  Nothing
# goes into the image: stage `test` builds build/libdrm-out/drmtest, the
# program of milestone M3 (drmModeGetResources() returns the virtual
# connector), for you to copy in and run.
#
# The library is STATIC by default (an archive is what the toolchain's
# link recipe handles with the least risk).  LIBDRM_LIBRARY=both also
# builds libdrm.so.2.124.0 (SONAME libdrm.so.2), the DSO that Xorg's
# dlopen()ed modules will link against.
#
# Usage:
#   sh ports/libdrm/build.sh               # every stage, in order
#   sh ports/libdrm/build.sh stage <name>  # one stage
#   sh ports/libdrm/build.sh list
#
# Environment / flags:
#   --prefix <dir>    toolchain prefix (default build/toolchain/usr)
#   --src <dir>       sources directory (default build/xorg-src)
#   --sysroot <dir>   install root (default build/xorg-native/sysroot)
#   --jobs <n>        parallel jobs (default nproc, capped at 8)
#   LIBDRM_VER        default 2.4.124 (the patch is per version)
#   LIBDRM_LIBRARY    static (default) | both
#   LIBDRM_MIRROR     default https://dri.freedesktop.org/libdrm
#
# Requirements: whatever `make mlibc` already needed (the toolchain and
# the mlibc sysroot must be there), plus meson >= 0.59 + ninja (the mlibc
# stages need them already), pkg-config, python3, patch, curl, tar + xz.
# A libdrm-2.4.124.tar.xz (or .tar.gz) already in the sources directory
# is used as it is, so the download can be skipped.
set -e

REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=$REPO/build/toolchain/usr
SYSROOT_T=$PREFIX/x86_64-prismos
SRC=$REPO/build/xorg-src
SYSROOT=$REPO/build/xorg-native/sysroot
WORK=$REPO/build/libdrm-native
OUT=$REPO/build/libdrm-out
JOBS=$(nproc 2>/dev/null || echo 2)
[ "$JOBS" -gt 8 ] && JOBS=8
LIBDRM_VER=${LIBDRM_VER:-2.4.124}
LIBDRM_LIBRARY=${LIBDRM_LIBRARY:-static}
LIBDRM_MIRROR=${LIBDRM_MIRROR:-https://dri.freedesktop.org/libdrm}
STAGE=all

while [ $# -gt 0 ]; do
	case "$1" in
	--prefix)  PREFIX=$2; SYSROOT_T=$2/x86_64-prismos; shift 2 ;;
	--src)     SRC=$2; shift 2 ;;
	--sysroot) SYSROOT=$2; shift 2 ;;
	--jobs)    JOBS=$2; shift 2 ;;
	stage)     STAGE=$2; shift 2 ;;
	list)      echo "fetch setup build install verify test"; exit 0 ;;
	*) echo "usage: $0 [--prefix dir] [--src dir] [--sysroot dir] [--jobs n] [stage <name>|list]" >&2; exit 2 ;;
	esac
done

D=libdrm-$LIBDRM_VER
PORT=$REPO/ports/libdrm
CC=$PORT/prism-cc
say() { printf '[libdrm/%s] %s\n' "$1" "$2"; }
die() { echo "ports/libdrm: $*" >&2; exit 1; }

case "$LIBDRM_LIBRARY" in
static|both) ;;
*) die "LIBDRM_LIBRARY must be 'static' or 'both', not '$LIBDRM_LIBRARY'" ;;
esac

# fetch_tar file url [url ...] -- same discipline as ports/nano and
# ports/xorg: --fail so an HTTP error page is never saved under the
# tarball's name, `tar tf` to reject anything that is not an archive.
fetch_tar() { # file url [url ...]
	f=$1
	shift
	for url in "$@"; do
		for resume in "-C -" "" ""; do
			if curl -sS -fL --retry 3 $resume -o "$f.part" "$url" \
				&& tar tf "$f.part" >/dev/null 2>&1; then
				mv "$f.part" "$f"
				return 0
			fi
			rm -f "$f.part"
			sleep 2
		done
	done
	die "$f: the download failed or is not a tarball (drop it into $SRC by hand to go offline)"
}

# The toolchain has to be there first: the driver compiles through
# x86_64-prismos-gcc and links libc.a out of the mlibc sysroot.
need_toolchain() {
	[ -x "$PREFIX/bin/x86_64-prismos-gcc" ] \
		|| die "no toolchain in $PREFIX -- run \`make mlibc\` first"
	[ -f "$SYSROOT_T/usr/lib/libc.a" ] \
		|| die "no libc.a in $SYSROOT_T -- run \`make mlibc\` first"
	# the driver reads these two
	export PRISM_PREFIX=$PREFIX PRISM_PORTSYS=$SYSROOT
	export PATH="$PREFIX/bin:$PATH"
}

# ---- stage: fetch -------------------------------------------------------------
# The tarball (a .tar.xz or .tar.gz already in the sources directory wins
# over the download), extracted, then the prismos patch -- idempotent: an
# already patched tree is recognised by the macro the patch introduces.
do_fetch() {
	mkdir -p "$SRC"
	cd "$SRC"
	if [ ! -f "$D/meson.build" ]; then
		f=
		for ext in tar.xz tar.gz; do
			[ -f "$D.$ext" ] && { f=$D.$ext; break; }
		done
		if [ -n "$f" ] && ! tar tf "$f" >/dev/null 2>&1; then
			say fetch "$f: not a readable tarball, refetching"
			rm -f "$f"
			f=
		fi
		if [ -z "$f" ]; then
			f=$D.tar.xz
			say fetch "$f"
			fetch_tar "$f" "$LIBDRM_MIRROR/$f"
		fi
		say fetch "extracting $f"
		tar xf "$f"
	fi
	patch=$PORT/patches/$D-prismos.patch
	[ -f "$patch" ] || die "no $patch -- the patch is per version, port it for $LIBDRM_VER"
	if grep -q __prismos__ "$D/xf86drm.h"; then
		say fetch "$D: prismos patch already in place"
	else
		patch -d "$D" -p1 --no-backup-if-mismatch < "$patch" >/dev/null \
			|| die "$D: the prismos patch does not apply"
		say fetch "$D: prismos patch applied"
	fi
	say fetch "sources ready ($D)"
}

# ---- stage: setup -------------------------------------------------------------
# The cross file and `meson setup`.  Every GPU driver is switched off (the
# intel one is `auto` on x86 and would otherwise insist on libpciaccess);
# an option the version's meson_options.txt does not have is skipped
# rather than handed to meson, which would reject it.
do_setup() {
	need_toolchain
	command -v meson >/dev/null || die "no meson on the host (pip install meson; >= 0.59)"
	command -v ninja >/dev/null || die "no ninja on the host"
	[ -f "$SRC/$D/meson.build" ] || die "no $SRC/$D -- run stage fetch first"
	grep -q __prismos__ "$SRC/$D/xf86drm.h" || die "$D is not patched -- run stage fetch first"
	mkdir -p "$WORK"

	nm=$PREFIX/bin/x86_64-prismos-nm
	{
		echo "[binaries]"
		echo "c = '$CC'"
		echo "ar = '$PREFIX/bin/x86_64-prismos-ar'"
		echo "ranlib = '$PREFIX/bin/x86_64-prismos-ranlib'"
		echo "strip = '$PREFIX/bin/x86_64-prismos-strip'"
		[ -x "$nm" ] && echo "nm = '$nm'"
		echo "pkg-config = 'pkg-config'"
		echo
		echo "[host_machine]"
		echo "system = 'prismos'"
		echo "cpu_family = 'x86_64'"
		echo "cpu = 'x86_64'"
		echo "endian = 'little'"
		echo
		echo "[properties]"
		echo "needs_exe_wrapper = true"
		# a cross build must not pick up the build machine's libraries
		# (atomic_ops, cunit, cairo are probed with required: false)
		echo "pkg_config_libdir = ['$SYSROOT/usr/lib/pkgconfig', '$SYSROOT/usr/share/pkgconfig']"
	} > "$WORK/cross.txt"

	optfile=
	for o in meson.options meson_options.txt; do
		[ -f "$SRC/$D/$o" ] && { optfile=$SRC/$D/$o; break; }
	done
	[ -n "$optfile" ] || die "$D has no meson options file"
	opts=
	for kv in intel=disabled radeon=disabled amdgpu=disabled nouveau=disabled \
		  vmwgfx=disabled omap=disabled exynos=disabled freedreno=disabled \
		  tegra=disabled vc4=disabled etnaviv=disabled cairo-tests=disabled \
		  man-pages=disabled valgrind=disabled freedreno-kgsl=false \
		  install-test-programs=false udev=false tests=false; do
		k=${kv%%=*}
		if grep -q "^[[:space:]]*'$k'," "$optfile"; then
			opts="$opts -D$kv"
		else
			say setup "option '$k' is not in $D -- skipped"
		fi
	done

	say setup "meson setup (cross, prefix /usr, default_library=$LIBDRM_LIBRARY)"
	rm -rf "$WORK/build"
	# shellcheck disable=SC2086
	meson setup "$WORK/build" "$SRC/$D" --cross-file "$WORK/cross.txt" \
		--prefix=/usr --libdir=lib --buildtype=release \
		-Ddefault_library="$LIBDRM_LIBRARY" -Db_lundef=false $opts \
		> "$WORK/setup.log" 2>&1 \
		|| { tail -n 40 "$WORK/setup.log" >&2; die "meson setup failed (log: $WORK/setup.log; build log: $WORK/build/meson-logs/meson-log.txt)"; }
	say setup "configured (log: $WORK/setup.log)"
}

# ---- stage: build -------------------------------------------------------------
do_build() {
	need_toolchain
	[ -f "$WORK/build/build.ninja" ] || die "not configured -- run stage setup first"
	say build "ninja -j$JOBS"
	ninja -C "$WORK/build" -j"$JOBS" > "$WORK/build.log" 2>&1 \
		|| { tail -n 40 "$WORK/build.log" >&2; die "build failed (log: $WORK/build.log)"; }
	[ -f "$WORK/build/libdrm.a" ] || die "no libdrm.a in $WORK/build after the build"
}

# ---- stage: install -----------------------------------------------------------
do_install() {
	need_toolchain
	[ -f "$WORK/build/libdrm.a" ] || die "not built -- run stage build first"
	mkdir -p "$SYSROOT"
	DESTDIR="$SYSROOT" meson install -C "$WORK/build" --no-rebuild \
		> "$WORK/install.log" 2>&1 \
		|| { tail -n 30 "$WORK/install.log" >&2; die "install failed (log: $WORK/install.log)"; }
	say install "$D -> $SYSROOT"
}

# ---- stage: verify ------------------------------------------------------------
# What the next ports and the test program rely on:
#   - the headers, the archive (and the DSO for LIBDRM_LIBRARY=both) and
#     libdrm.pc are in the sysroot;
#   - pkg-config, pointed at the sysroot the cross way, resolves libdrm
#     at the right version;
#   - every symbol of the version's core-symbols.txt (212 for 2.4.124,
#     the list upstream's own test enforces) is defined by the library.
do_verify() {
	need_toolchain
	for h in xf86drm.h xf86drmMode.h libdrm/drm.h libdrm/drm_fourcc.h libdrm/drm_mode.h; do
		[ -f "$SYSROOT/usr/include/$h" ] || die "verify: $SYSROOT/usr/include/$h is missing -- run stage install"
	done
	lib=$SYSROOT/usr/lib/libdrm.a
	[ -f "$lib" ] || die "verify: $lib is missing -- run stage install"
	say verify "headers and libdrm.a are in the sysroot"

	command -v pkg-config >/dev/null || die "verify: no pkg-config on the host"
	unset PKG_CONFIG_PATH
	export PKG_CONFIG_SYSROOT_DIR=$SYSROOT
	export PKG_CONFIG_LIBDIR=$SYSROOT/usr/share/pkgconfig:$SYSROOT/usr/lib/pkgconfig
	v=$(pkg-config --modversion libdrm) || die "verify: pkg-config cannot resolve libdrm in $SYSROOT"
	[ "$v" = "$LIBDRM_VER" ] || die "verify: libdrm.pc says $v, expected $LIBDRM_VER"
	say verify "pkg-config libdrm $v"

	nm=$PREFIX/bin/x86_64-prismos-nm
	[ -x "$nm" ] || nm=nm
	mkdir -p "$WORK"
	"$nm" -g --defined-only "$lib" 2>/dev/null \
		| awk '$2 ~ /^[TDBR]$/ { print $3 }' | sort -u > "$WORK/defined.txt"
	sort -u "$SRC/$D/core-symbols.txt" > "$WORK/core.txt"
	miss=$(comm -13 "$WORK/defined.txt" "$WORK/core.txt")
	[ -z "$miss" ] || die "verify: libdrm.a lacks core symbols: $(echo $miss | tr '\n' ' ')"
	say verify "libdrm.a defines all $(wc -l < "$WORK/core.txt") core symbols"

	if [ "$LIBDRM_LIBRARY" = both ]; then
		dso=$SYSROOT/usr/lib/libdrm.so.2.${LIBDRM_VER##*.}.0
		[ -f "$dso" ] || die "verify: $dso is missing (LIBDRM_LIBRARY=both)"
		readelf=$PREFIX/bin/x86_64-prismos-readelf
		[ -x "$readelf" ] || readelf=readelf
		"$readelf" -d "$dso" | grep -q 'SONAME.*libdrm\.so\.2' \
			|| die "verify: $dso has no SONAME libdrm.so.2"
		say verify "libdrm.so.2.${LIBDRM_VER##*.}.0: SONAME libdrm.so.2"
	fi
}

# ---- stage: test --------------------------------------------------------------
# drmtest (milestone M3) compiled and linked with the driver against the
# installed library: a static PIE, stripped.  Linking it is the closure
# check -- every libc symbol libdrm uses must be answered by libc.a.  The
# program is NOT put into the image; copy build/libdrm-out/drmtest in and
# run it under QEMU: `drmtest /dev/dri/card0`.
do_test() {
	need_toolchain
	[ -f "$SYSROOT/usr/lib/libdrm.a" ] || die "no libdrm.a in $SYSROOT -- run stage install first"
	mkdir -p "$OUT"
	"$CC" -O2 -Wall -I"$SYSROOT/usr/include" -I"$SYSROOT/usr/include/libdrm" \
		-c "$PORT/drmtest/drmtest.c" -o "$OUT/drmtest.o" \
		|| die "drmtest.c does not compile against the installed headers"
	"$CC" -o "$OUT/drmtest" "$OUT/drmtest.o" -L"$SYSROOT/usr/lib" -ldrm \
		|| die "drmtest does not link against libdrm.a + libc.a"
	"$PREFIX/bin/x86_64-prismos-strip" "$OUT/drmtest"
	size=$(wc -c < "$OUT/drmtest")
	# the kernel reads an executable whole, up to 4 MiB
	[ "$size" -lt 4194304 ] || die "drmtest is $size bytes, over the kernel's 4 MiB whole-file read"
	say test "$OUT/drmtest ($size bytes, static PIE) -- copy it into the image and run it for M3"
}

case "$STAGE" in
	all)     do_fetch; do_setup; do_build; do_install; do_verify; do_test ;;
	fetch)   do_fetch ;;
	setup)   do_setup ;;
	build)   do_build ;;
	install) do_install ;;
	verify)  do_verify ;;
	test)    do_test ;;
	*) echo "unknown stage: $STAGE (try: list)" >&2; exit 2 ;;
esac
