#!/bin/sh
#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# The Xorg port, step 1 of docs/XORG.md section 3: util-macros, xorgproto
# and xtrans -- the three packages every later X port (libdrm excepted)
# configures against.  None of them compiles anything for the target:
#
#   util-macros   the autoconf macros (xorg-macros.m4) and xorg-macros.pc
#   xorgproto     the protocol headers (X11/X.h, X11/Xproto.h,
#                 X11/extensions/*.h) and the xproto.pc / xextproto.pc /
#                 randrproto.pc ... files that name them
#   xtrans        the transport layer, shipped as sources and headers that
#                 libX11 / libxcb-less clients and the server #include
#                 (X11/Xtrans/*), plus xtrans.pc
#
# They are architecture-independent, so they are configured WITHOUT --host
# and with the host's own tools: a triplet would need config.sub taught
# x86_64-prismos for no gain (no object code is produced), and the
# target compiler is not required -- this port does not depend on
# `make mlibc`.  What matters is where they land: the port's own sysroot,
# build/xorg-native/sysroot, with --prefix=/usr and DESTDIR (the .pc files
# keep prefix=/usr, and the later ports find them the usual cross way,
# PKG_CONFIG_SYSROOT_DIR + PKG_CONFIG_LIBDIR below).  The mlibc sysroot
# stays pristine, the way ports/nano keeps it.  Nothing is installed into
# the image: headers and macros are build-time only.
#
# Order: util-macros, xorgproto, xtrans (the conventional X.Org order; the
# release tarballs carry their own generated configure, so nothing here
# depends on it, but later ports' autoreconf needs the macros first).
#
# Usage:
#   sh ports/xorg/build.sh               # every stage, in order
#   sh ports/xorg/build.sh stage <name>  # one stage
#   sh ports/xorg/build.sh list
#
# Environment / flags:
#   --src <dir>         sources directory (default build/xorg-src)
#   --sysroot <dir>     install root (default build/xorg-native/sysroot)
#   --jobs <n>          parallel jobs (default nproc, capped at 8)
#   UTIL_MACROS_VER     default 1.20.2
#   XORGPROTO_VER       default 2024.1
#   XTRANS_VER          default 1.6.0
#   XORG_MIRROR         default https://xorg.freedesktop.org/releases/individual
#   XORG_MIRROR2        default https://www.x.org/releases/individual
#
# Offline: a tarball already sitting in the sources directory is used as
# it is (stage fetch only downloads what is missing), so the three
# .tar.xz files can be dropped into build/xorg-src by hand.
#
# Requirements: a host C compiler and make, pkg-config (or pkgconf),
# curl, tar + xz.  autoconf / automake are NOT needed (the tarballs ship
# a generated configure).
set -e

REPO=$(cd "$(dirname "$0")/../.." && pwd)
SRC=$REPO/build/xorg-src
SYSROOT=$REPO/build/xorg-native/sysroot
JOBS=$(nproc 2>/dev/null || echo 2)
[ "$JOBS" -gt 8 ] && JOBS=8
UTIL_MACROS_VER=${UTIL_MACROS_VER:-1.20.2}
XORGPROTO_VER=${XORGPROTO_VER:-2024.1}
XTRANS_VER=${XTRANS_VER:-1.6.0}
XORG_MIRROR=${XORG_MIRROR:-https://xorg.freedesktop.org/releases/individual}
XORG_MIRROR2=${XORG_MIRROR2:-https://www.x.org/releases/individual}
STAGE=all

while [ $# -gt 0 ]; do
	case "$1" in
	--src)     SRC=$2; shift 2 ;;
	--sysroot) SYSROOT=$2; shift 2 ;;
	--jobs)    JOBS=$2; shift 2 ;;
	stage)     STAGE=$2; shift 2 ;;
	list)      echo "fetch macros proto xtrans verify"; exit 0 ;;
	*) echo "usage: $0 [--src dir] [--sysroot dir] [--jobs n] [stage <name>|list]" >&2; exit 2 ;;
	esac
done

# Out-of-tree build directories, wiped per package so a stale configure
# cache never survives a version bump.
WORK=$(dirname "$SYSROOT")/obj
say() { printf '[xorg/%s] %s\n' "$1" "$2"; }
die() { echo "ports/xorg: $*" >&2; exit 1; }

# fetch_tar file url [url ...] -- the first URL that yields a readable
# tarball wins (same discipline as ports/nano: --fail so an HTTP error
# page is never saved under the tarball's name, `tar tf` to reject
# anything that is not an archive, a failed attempt drops the .part).
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

# ---- stage: fetch -------------------------------------------------------------
# fetch_pkg <mirror subdirectory>/<package>-<version>: X.Org keeps protocol
# packages under proto/, libraries under lib/ and the build helpers under
# util/.  A .tar.xz or .tar.gz already in the sources directory is used as
# it is; the download only happens when neither is there.  Each stage
# below fetches its own package, so `stage proto` works with xorgproto
# alone.
fetch_pkg() {
	path=$1; d=${path##*/}
	mkdir -p "$SRC"
	cd "$SRC"
	[ -f "$d/configure" ] && return 0
	f=
	for ext in tar.xz tar.gz; do
		[ -f "$d.$ext" ] && { f=$d.$ext; break; }
	done
	if [ -n "$f" ] && [ ! -d "$d" ] && ! tar tf "$f" >/dev/null 2>&1; then
		say fetch "$f: not a readable tarball, refetching"
		rm -f "$f"
		f=
	fi
	if [ -z "$f" ]; then
		f=$d.tar.xz
		say fetch "$f"
		fetch_tar "$f" "$XORG_MIRROR/$path.tar.xz" "$XORG_MIRROR2/$path.tar.xz"
	fi
	if [ ! -d "$d" ]; then
		say fetch "extracting $f"
		tar xf "$f"
	fi
}

do_fetch() {
	fetch_pkg "util/util-macros-$UTIL_MACROS_VER"
	fetch_pkg "proto/xorgproto-$XORGPROTO_VER"
	fetch_pkg "lib/xtrans-$XTRANS_VER"
	say fetch "sources ready (util-macros-$UTIL_MACROS_VER, xorgproto-$XORGPROTO_VER, xtrans-$XTRANS_VER)"
}

# need_src dir -- the stages after fetch take the sources as they are.
need_src() {
	[ -f "$SRC/$1/configure" ] || die "no $SRC/$1/configure -- run stage fetch first"
}

# autotools_install name configure-args... -- the three packages follow
# the same recipe: out-of-tree configure for --prefix=/usr, make, and
# make install into the sysroot.  Documentation is switched off (xmlto /
# fop / xsltproc are not requirements of this port); an option a
# package's configure does not know is only a warning, so one list serves
# all three.
autotools_install() {
	name=$1
	shift
	need_src "$name"
	rm -rf "$WORK/$name"
	mkdir -p "$WORK/$name" "$SYSROOT"
	( cd "$WORK/$name" \
		&& "$SRC/$name/configure" --prefix=/usr \
			--disable-docs --disable-specs \
			--without-xmlto --without-fop --without-xsltproc --without-asciidoctor \
			"$@" >configure.log 2>&1 ) \
		|| { tail -n 25 "$WORK/$name/configure.log" >&2; die "$name: configure failed (log: $WORK/$name/configure.log)"; }
	( cd "$WORK/$name" \
		&& make -j"$JOBS" >make.log 2>&1 \
		&& make DESTDIR="$SYSROOT" install >install.log 2>&1 ) \
		|| { tail -n 25 "$WORK"/"$name"/make.log "$WORK"/"$name"/install.log >&2; die "$name: build / install failed (logs in $WORK/$name)"; }
}

# ---- stage: macros ------------------------------------------------------------
do_macros() {
	fetch_pkg "util/util-macros-$UTIL_MACROS_VER"
	autotools_install "util-macros-$UTIL_MACROS_VER"
	[ -f "$SYSROOT/usr/share/aclocal/xorg-macros.m4" ] \
		|| die "util-macros installed no xorg-macros.m4 under $SYSROOT/usr/share/aclocal"
	say macros "util-macros-$UTIL_MACROS_VER -> $SYSROOT"
}

# ---- stage: proto -------------------------------------------------------------
do_proto() {
	fetch_pkg "proto/xorgproto-$XORGPROTO_VER"
	autotools_install "xorgproto-$XORGPROTO_VER"
	[ -f "$SYSROOT/usr/include/X11/Xproto.h" ] \
		|| die "xorgproto installed no X11/Xproto.h under $SYSROOT/usr/include"
	say proto "xorgproto-$XORGPROTO_VER -> $SYSROOT"
}

# ---- stage: xtrans ------------------------------------------------------------
do_xtrans() {
	fetch_pkg "lib/xtrans-$XTRANS_VER"
	autotools_install "xtrans-$XTRANS_VER"
	[ -f "$SYSROOT/usr/include/X11/Xtrans/Xtrans.h" ] \
		|| die "xtrans installed no X11/Xtrans/Xtrans.h under $SYSROOT/usr/include"
	say xtrans "xtrans-$XTRANS_VER -> $SYSROOT"
}

# ---- stage: verify ------------------------------------------------------------
# What the later ports will rely on, checked the way they will use it:
#   - the files are in the sysroot;
#   - pkg-config, pointed at the sysroot the cross way, resolves the
#     package names the next steps ask for (xproto, xextproto, randrproto,
#     kbproto, inputproto, xtrans, xorg-macros);
#   - a host compiler parses the headers from the sysroot alone (-fsyntax-only:
#     they are architecture-independent).
do_verify() {
	inc=$SYSROOT/usr/include
	for h in X11/X.h X11/Xproto.h X11/extensions/randr.h; do
		[ -f "$inc/$h" ] || die "verify: $inc/$h is missing -- run stage proto first"
	done
	say verify "xorgproto headers are in the sysroot"
	mods="xproto xextproto randrproto kbproto inputproto"
	# util-macros and xtrans are checked when they are installed: a run
	# that only has xorgproto says so instead of failing.
	if [ -f "$SYSROOT/usr/share/aclocal/xorg-macros.m4" ]; then
		mods="$mods xorg-macros"
	else
		say verify "util-macros not installed -- skipped"
	fi
	if [ -f "$inc/X11/Xtrans/Xtrans.h" ]; then
		mods="$mods xtrans"
	else
		say verify "xtrans not installed -- skipped"
	fi

	command -v pkg-config >/dev/null || die "verify: no pkg-config on the host"
	unset PKG_CONFIG_PATH
	export PKG_CONFIG_SYSROOT_DIR=$SYSROOT
	export PKG_CONFIG_LIBDIR=$SYSROOT/usr/share/pkgconfig:$SYSROOT/usr/lib/pkgconfig
	for m in $mods; do
		v=$(pkg-config --modversion "$m") || die "verify: pkg-config cannot resolve '$m' in $SYSROOT"
		say verify "pkg-config $m $v"
	done

	cc=${CC:-cc}
	command -v "$cc" >/dev/null || die "verify: no host C compiler ($cc)"
	printf '#include <X11/X.h>\n#include <X11/Xproto.h>\n#include <X11/extensions/randr.h>\nint main(void) { return X_PROTOCOL - 11; }\n' \
		| "$cc" -fsyntax-only -x c -I"$inc" - \
		|| die "verify: the host compiler cannot parse the protocol headers"
	say verify "X11/Xproto.h + X11/extensions/randr.h compile from the sysroot"
}

case "$STAGE" in
	all)    do_fetch; do_macros; do_proto; do_xtrans; do_verify ;;
	fetch)  do_fetch ;;
	macros) do_macros ;;
	proto)  do_proto ;;
	xtrans) do_xtrans ;;
	verify) do_verify ;;
	*) echo "unknown stage: $STAGE (try: list)" >&2; exit 2 ;;
esac
