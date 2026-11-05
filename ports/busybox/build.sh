#!/bin/sh
#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# Phase 3.4 -- BusyBox and a userspace shell: the port proper.  The
# roadmap's words, one by one:
#
#   BusyBox port          busybox 1.37.0, fetched once into
#                         build/toolchain-src (the directory the toolchain
#                         and binutils ports already share), built with the
#                         port's committed config
#                         (busybox-1.37.0-prismos.config: the roadmap's own
#                         applet list -- ash ls cat echo mkdir rm cp mv,
#                         then ps mount df grep sed awk tar vi -- plus the
#                         small utilities a session leans on: pwd true false
#                         test printf env sleep uname ln touch rmdir wc
#                         head tail id whoami basename dirname readlink)
#   minimal config        an allnoconfig seed turned into a settled
#                         .config, committed here so the port is
#                         deterministic: same tree, same config, same binary
#   static build          one static PIE (CONFIG_STATIC=y, linked as the
#                         kernel's loader expects through the prism-cc
#                         driver) against the mlibc sysroot, stripped under
#                         the 4 MiB whole-file cap
#   verify.sh extended    the port's verify stage runs verify.sh's custom
#                         mode on the unstripped binary with the syscall
#                         diet traced under prism_emu; tests/busybox_test.py
#                         (make busyboxtest) is the runtime proof
#
# The driver is ports/binutils' prism-cc with one busybox-specific answer:
# `-lm` resolves to nothing (mlibc's libc.a carries the musl-derived math
# since the ansi option; busybox's makefile adds -lm unconditionally "for
# dc, awk, ntpd"), and the `-lcrypt` / `-lrt` probes fail honestly through
# the same resolve rule, so those libraries stay out the way they would on
# any libc that has no separate crypt / rt.
#
# Usage:
#   sh ports/busybox/build.sh               # every stage, in order
#   sh ports/busybox/build.sh stage <name>  # one stage
#   sh ports/busybox/build.sh list
#
# Environment / flags:
#   --prefix <dir>    toolchain prefix (default build/toolchain/usr)
#   --src <dir>       sources directory (default build/toolchain-src,
#                     shared with ports/toolchain: tarballs fetched once)
#   --jobs <n>        parallel jobs (default nproc, capped at 8)
#   BUSYBOX_VER       default 1.37.0
#   MIRROR            default https://busybox.net/downloads
#
# Requirements: whatever `make mlibc` already needed (the toolchain prefix
# with the mlibc sysroot -- and, since this phase, its linux + bsd options
# for mount(2) / statfs(2) / the mntent family), plus the busybox tarball
# (stage fetch).  The one binary lands in build/busybox-out; the applet
# symlinks are the image's business (the Makefile's root rule).
set -e

REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=$REPO/build/toolchain/usr
SYSROOT=$PREFIX/x86_64-prismos
SRC=$REPO/build/toolchain-src
WORK=$REPO/build/busybox-build
OUT=$REPO/build/busybox-out
JOBS=$(nproc 2>/dev/null || echo 2)
[ "$JOBS" -gt 8 ] && JOBS=8
BUSYBOX_VER=${BUSYBOX_VER:-1.37.0}
MIRROR=${MIRROR:-https://busybox.net/downloads}
# PrismOS is x86_64, full stop -- the only ARCH busybox needs told.
ARCH=x86_64
STAGE=all

while [ $# -gt 0 ]; do
        case "$1" in
        --prefix) PREFIX=$2; SYSROOT=$2/x86_64-prismos; shift 2 ;;
        --src)    SRC=$2; shift 2 ;;
        --jobs)   JOBS=$2; shift 2 ;;
        stage)    STAGE=$2; shift 2 ;;
        list)     echo "fetch cc config build stage verify"; exit 0 ;;
        *) echo "usage: $0 [--prefix dir] [--src dir] [--jobs n] [stage <name>|list]" >&2; exit 2 ;;
        esac
done

say() { printf '[busybox/%s] %s\n' "$1" "$2"; }

# The toolchain has to be there first: the driver compiles through
# x86_64-prismos-gcc and the sandwich links libc.a out of the sysroot --
# with the linux + bsd options this phase turned on (mount / statfs /
# mntent are what the `mount` and `df` applets parse their flags through).
[ -x "$PREFIX/bin/x86_64-prismos-gcc" ] || {
        echo "ports/busybox: no toolchain in $PREFIX -- run \`make mlibc\` first" >&2
        exit 1
}
[ -f "$SYSROOT/usr/lib/libc.a" ] || {
        echo "ports/busybox: no libc.a in the sysroot -- run \`make mlibc\` first" >&2
        exit 1
}

# ---- stage: fetch -------------------------------------------------------------
# The tarball, once, into the shared sources directory; no patch -- the
# port's whole influence on the tree is the committed config.
do_fetch() {
        mkdir -p "$SRC"
        cd "$SRC"
        f=busybox-$BUSYBOX_VER.tar.bz2
        if [ ! -f "$f" ]; then
                say fetch "$f"
                curl -sS --retry 3 -C - -o "$f.part" "$MIRROR/$f" && mv "$f.part" "$f"
        fi
        if [ ! -d "busybox-$BUSYBOX_VER" ]; then
                say fetch "extracting $f"
                tar xf "$f"
        fi
        say fetch "sources ready (busybox-$BUSYBOX_VER)"
}

# ---- stage: cc ----------------------------------------------------------------
# prism-cc, the driver busybox's make talks to as CC: a compile goes through
# x86_64-prismos-gcc with the do_prog discipline, a link assembles the
# static-PIE sandwich (ports/binutils' driver, one busybox answer added:
# -lm maps to nothing -- mlibc's libc.a carries the math).  Busybox's own
# link goes through scripts/trylink, whose compile-and-link probes and
# archive-dropping retries all funnel into this driver's two modes.
do_cc() {
        mkdir -p "$WORK"
        cat > "$WORK/prism-cc" <<'EOF'
#!/bin/sh
# prism-cc -- the compiler driver of the busybox port (generated by
# ports/busybox/build.sh, ports/binutils' driver with the -lm answer):
# busybox's make and its scripts/trylink talk to it as CC, and it answers
# with PrismOS objects and static PIEs.
#   compile (-c):  x86_64-prismos-gcc, the do_prog discipline of
#                  ports/toolchain turned into driver form
#   preprocess (-E) and the dependency generators (-M...): the same
#                  discipline, no output file
#   link (-o):     the sandwich the kernel's loader expects -- the same
#                  link tools/prism.py makes for every app
set -e
REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=$REPO/build/toolchain/usr
SYSROOT=$PREFIX/x86_64-prismos
CCRAW=$PREFIX/bin/x86_64-prismos-gcc
LD=$PREFIX/bin/x86_64-prismos-ld
CRTBEGIN=$($CCRAW -print-file-name=crtbeginS.o)
CRTEND=$($CCRAW -print-file-name=crtendS.o)
LIBGCC=$($CCRAW -print-libgcc-file-name)
GCCINC=$($CCRAW -print-file-name=include)
BASE="-O2 -ffreestanding -fPIE -fno-stack-protector -fno-asynchronous-unwind-tables -fno-math-errno"

compile() {
        exec "$CCRAW" $BASE -nostdinc -isystem "$GCCINC" \
                -isystem "$SYSROOT/usr/include" "$@"
}

# ---- the link: the sandwich of tools/prism.py's compile_and_link ----
# trylink hands the driver objects plus -Wl,* flags and -l libraries; the
# flags drop out (the sandwich owns the geometry), the libraries resolve
# against the -L list -- with one busybox answer: -lm is nothing, mlibc's
# libc.a carries the musl-derived math the ansi option builds.
#
# Kbuild's partial links (the built-in.o of every directory) arrive here
# too: busybox's makefile spells LD as "$(CC) -nostdlib", so the driver
# sees its own name with -r -- that one is a plain relocatable link, the
# raw cross ld's own job, no sandwich (there is no _start to build).
link() {
        case " $* " in
        *" -r "*)
                out= objs=
                while [ $# -gt 0 ]; do
                        case "$1" in
                        -o)   out=$2; shift 2 ;;
                        *.o)  objs="$objs $1"; shift ;;
                        *)    shift ;;
                        esac
                done
                [ -n "$out" ] || out=a.out
                exec "$LD" -r -o "$out" $objs
                ;;
        esac
        out= objs= archives= libs= ldirs= srcs= tmpobjs=
        while [ $# -gt 0 ]; do
                case "$1" in
                -o)         out=$2; shift 2 ;;
                --version|-v|-V|-qversion|-dumpversion|-dumpmachine|-print-*)
                            exec "$CCRAW" "$@" ;;
                -I*|-D*|-U*|-g*|-O*|-f*|-W*|-m*|-pedantic*|-std*|-c|-pedantic)
                            shift ;;
                -L)         ldirs="$ldirs $2"; shift 2 ;;
                -L*)        ldirs="$ldirs ${1#-L}"; shift ;;
                -l)         libs="$libs $2"; shift 2 ;;
                -l*)        libs="$libs ${1#-l}"; shift ;;
                *.lo)       objs="$objs ${1%.lo}.o"; shift ;;
                *.o)        objs="$objs $1"; shift ;;
                *.a)        archives="$archives $1"; shift ;;
                *.c)        srcs="$srcs $1"; shift ;;
                *)          shift ;;
                esac
        done
        # a compiler's default output name, the way trylink's probes use it
        [ -n "$out" ] || out=a.out

        # trylink's check_cc probes compile-and-link a temp .c in one go
        for f in $srcs; do
                o=$(mktemp "$(dirname "$0")/prism-cc.XXXXXX")
                if ! "$CCRAW" $BASE -nostdinc -isystem "$GCCINC" \
                                -isystem "$SYSROOT/usr/include" -c "$f" -o "$o"; then
                        rm -f "$o"
                        exit 1
                fi
                objs="$objs $o"
                tmpobjs="$tmpobjs $o"
        done
        [ -n "$objs" ] || { echo "prism-cc: nothing to link" >&2; exit 1; }

        # -lfoo resolves against the -L list (nothing but libc.a and the
        # build's own libraries can appear); -lm maps to nothing -- the
        # math lives inside mlibc's libc.a, there is no libm.a to find, and
        # busybox's makefile adds it unconditionally ("for dc, awk, ntpd").
        resolved=
        for l in $libs; do
                [ "$l" = m ] && continue
                found=
                for d in $ldirs; do
                        if [ -f "$d/lib$l.a" ]; then found="$d/lib$l.a"; break; fi
                done
                [ -n "$found" ] || { echo "prism-cc: cannot resolve -l$l" >&2; exit 1; }
                resolved="$resolved $found"
        done

        # libc.a leads the group (ports/binutils' rule): anything busybox
        # and libbb ask the C library for resolves there before an archive
        # scanned later could collide with it.
        "$LD" -m elf_x86_64 -pie --no-dynamic-linker -z separate-code \
                -z max-page-size=4096 -z noexecstack -z norelro -e _start \
                -o "$out" \
                "$SYSROOT/usr/lib/Scrt1.o" "$SYSROOT/usr/lib/crti.o" \
                "$CRTBEGIN" \
                $objs \
                --start-group "$SYSROOT/usr/lib/libc.a" \
                $archives $resolved "$LIBGCC" --end-group \
                "$CRTEND" "$SYSROOT/usr/lib/crtn.o"
        rc=$?
        rm -f $tmpobjs
        exit $rc
}

case " $* " in
*" -E "*|*" -M"*) compile "$@" ;;
*" -c "*)          compile "$@" ;;
*)                 link "$@" ;;
esac
EOF
        chmod +x "$WORK/prism-cc"
        say cc "$WORK/prism-cc (compile: $PREFIX/bin/x86_64-prismos-gcc, link: the static-PIE sandwich, -lm: libc.a's own)"
}

# ---- stage: config ------------------------------------------------------------
# The committed config, settled: `oldconfig` walks whatever the version's
# Kconfig wants answered and takes the defaults -- the committed file is the
# port's decisions, this stage only makes the tree's generated headers
# (include/config/, include/applet_tables.h) match it.
do_config() {
        [ -d "$SRC/busybox-$BUSYBOX_VER" ] || do_fetch
        [ -x "$WORK/prism-cc" ] || do_cc
        cd "$SRC/busybox-$BUSYBOX_VER"
        cfg=$REPO/ports/busybox/busybox-$BUSYBOX_VER-prismos.config
        [ -f "$cfg" ] || {
                echo "ports/busybox: $cfg missing (the committed config IS the port)" >&2
                exit 1
        }
        say config "busybox-$BUSYBOX_VER-prismos.config"
        cp "$cfg" .config
        yes '' | make oldconfig > "$WORK/oldconfig.log" 2>&1 \
                || { tail -20 "$WORK/oldconfig.log"; exit 1; }
}

# ---- stage: build --------------------------------------------------------------
# ARCH tells busybox's make nothing it does not already know (x86_64 here);
# CC is the driver; SKIP_STRIP keeps the unstripped binary for the verify
# stage (the cross strip does its own stage).  The Kbuild host tools
# (applet_tables, usage, fixdep, kconfig's conf) build with HOSTCC, the
# machine running this script -- only the libc-facing objects go through
# the driver.
do_build() {
        [ -f "$SRC/busybox-$BUSYBOX_VER/.config" ] || do_config
        cd "$SRC/busybox-$BUSYBOX_VER"
        say build "make -j$JOBS (CC: the prism-cc driver, STATIC)"
        make -j"$JOBS" ARCH=$ARCH CC="$WORK/prism-cc" SKIP_STRIP=y \
                > "$WORK/build.log" 2>&1 \
                || { tail -40 "$WORK/build.log"; exit 1; }
        [ -f busybox_unstripped ] || {
                echo "ports/busybox: busybox_unstripped missing" >&2; exit 1; }
        say build "busybox_unstripped is a PrismOS static PIE ($(du -h busybox_unstripped | cut -f1))"
}

# ---- stage: stage --------------------------------------------------------------
# The one binary, stripped with the cross strip (the kernel reads whole
# files: MAX_ELF_FILE is 4 MiB) into build/busybox-out.  The applets are
# symlinks to it -- the image's business, not the port's.
do_stage() {
        do_build
        mkdir -p "$OUT"
        say stage "stripping busybox"
        $PREFIX/bin/x86_64-prismos-strip --strip-all -o "$OUT/busybox" \
                "$SRC/busybox-$BUSYBOX_VER/busybox_unstripped"
        sz=$(wc -c < "$OUT/busybox")
        if [ "$sz" -gt $((4 * 1024 * 1024)) ]; then
                echo "ports/busybox: $OUT/busybox is $sz bytes -- over the kernel's 4 MiB cap" >&2
                exit 1
        fi
        say stage "$OUT/busybox ($(du -h "$OUT/busybox" | cut -f1), under the 4 MiB cap)"
}

# ---- stage: verify -------------------------------------------------------------
# verify.sh's custom mode on the unstripped binary (the symbols the diet
# checks live in), exactly the way tools/prism.py verifies an app and
# ports/binutils verifies its five.  The diet is the one traced under
# prism_emu (tests/busybox_test.py drives the same scenarios): the mlibc
# startup pair, the stdio trio, the directory walk, the stat family, the
# fd pair / close -- and the new phase 3.4 calls the applets lean on:
# fchdir / fsync / chmod / utimensat (touch's, cp's and cd's half of the
# phase) and wait4, `time`'s own report of a child's rusage.  The symbols:
# the shell's own entry (ash_main -- the applet the session spawns) and
# the dispatch table every applet walks (applet_main, 1.37's
# busybox_main).
do_verify() {
        [ -f "$SRC/busybox-$BUSYBOX_VER/busybox_unstripped" ] || do_build
        diet_busybox="1 2 13 14 15 16 19 20 24 25 27 28 29 36 47 58 62 63 64 66 68"
        say verify "verify.sh busybox (loader contract + syscall diet)"
        PRISM_VERIFY_SYSCALLS="$diet_busybox" \
        PRISM_VERIFY_SYMBOLS="ash_main applet_main" \
        sh "$REPO/ports/toolchain/verify.sh" \
                "$SRC/busybox-$BUSYBOX_VER/busybox_unstripped" "$SYSROOT" custom
}

case "$STAGE" in
        all)      do_fetch; do_cc; do_config; do_build; do_stage; do_verify ;;
        fetch)    do_fetch ;;
        cc)       do_cc ;;
        config)   do_config ;;
        build)    do_build ;;
        stage)    do_stage ;;
        verify)   do_verify ;;
        *) echo "unknown stage: $STAGE (try: list)" >&2; exit 2 ;;
esac
