#!/bin/sh
#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# Phase 4.1 -- the binutils utilities as PrismOS programs: `ar`, `ranlib`,
# `strip`, `objdump` and `nm`, cross-built for x86_64-prismos and installed
# into the image.  The roadmap's words, one by one:
#
#   complete binutils in the image
#                 the five utilities land in the image's /usr/bin as
#                 PrismOS executables -- the first real software the
#                 self-hosting phases (5.1 and on) will run on the target;
#                 `as` and `ld` stay phase 5.1's business, the utilities are
#                 this phase's
#   cross-built   binutils 2.44, the same tree and the same triplet patch
#                 ports/toolchain already fetches, configured here with
#                 --host=x86_64-prismos --target=x86_64-prismos: host and
#                 target are PrismOS, build is the machine running this
#                 script -- a plain cross compile, the way SerenityOS builds
#                 its binutils for serenity
#   installed     staged (stripped, under the kernel's 4 MiB whole-file
#                 read) into build/binutils-out, then into the image's
#                 /usr/bin by the Makefile's root rule (`make root`)
#   used by the builds
#                 the port's own build runs on the cross binutils: AR and
#                 RANLIB archive libbfd / libopcodes / libsframe / zlib,
#                 the cross strip stages the five binaries; and verify.sh
#                 (phase 4.1's wiring) disassembles every program it
#                 checks with the prefixed objdump / nm / readelf when the
#                 toolchain has them
#
# The one addition to binutils' own build is `prism-cc`, a mini driver that
# answers to the compiler name configure looks for: it compiles through
# x86_64-prismos-gcc with the do_prog discipline (the mlibc sysroot's
# includes, the freestanding flag set every app builds with) and links the
# static-PIE sandwich the kernel's loader expects (Scrt1 / crti /
# crtbeginS / the objects / libc.a / crtendS / crtn, -z separate-code
# -z max-page-size=4096).  Both toolchain paths work unchanged: the
# bootstrap wrappers (the fast path, `make mlibc`) behind the same names,
# or build.sh's real cross tools.
#
# Usage:
#   sh ports/binutils/build.sh               # every stage, in order
#   sh ports/binutils/build.sh stage <name>  # one stage
#   sh ports/binutils/build.sh list
#
# Environment / flags:
#   --prefix <dir>    toolchain prefix (default build/toolchain/usr)
#   --src <dir>       sources directory (default build/toolchain-src,
#                     shared with ports/toolchain: the same tarball, the
#                     same patch, fetched once)
#   --jobs <n>        parallel jobs (default nproc, capped at 8)
#   BINUTILS_VER      default 2.44
#   MIRROR            default https://mirrors.kernel.org/gnu
#
# Requirements: whatever `make mlibc` already needed (the toolchain prefix
# with the mlibc sysroot), plus the binutils tarball (stage fetch).  The
# five binaries land in build/binutils-out, each one verified against the
# loader's contract by ports/toolchain/verify.sh.
set -e

REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=$REPO/build/toolchain/usr
SYSROOT=$PREFIX/x86_64-prismos
SRC=$REPO/build/toolchain-src
WORK=$REPO/build/binutils-native
OUT=$REPO/build/binutils-out
JOBS=$(nproc 2>/dev/null || echo 2)
[ "$JOBS" -gt 8 ] && JOBS=8
BINUTILS_VER=${BINUTILS_VER:-2.44}
MIRROR=${MIRROR:-https://mirrors.kernel.org/gnu}
# The five utilities, in image naming; the build tree's names differ
# (nm-new, strip-new -- the "objcopy and strip should be the same program"
# trick) so the map lives in one place.
TOOLS="ar ranlib nm objdump strip"
BUILD_NAME_NM=nm-new
BUILD_NAME_STRIP=strip-new
STAGE=all

while [ $# -gt 0 ]; do
        case "$1" in
        --prefix) PREFIX=$2; SYSROOT=$2/x86_64-prismos; shift 2 ;;
        --src)    SRC=$2; shift 2 ;;
        --jobs)   JOBS=$2; shift 2 ;;
        stage)    STAGE=$2; shift 2 ;;
        list)     echo "fetch cc configure build stage verify"; exit 0 ;;
        *) echo "usage: $0 [--prefix dir] [--src dir] [--jobs n] [stage <name>|list]" >&2; exit 2 ;;
        esac
done

PATCHES=$REPO/ports/toolchain/patches
say() { printf '[binutils/%s] %s\n' "$1" "$2"; }

# The toolchain has to be there first: the driver compiles through
# x86_64-prismos-gcc and the sandwich links libc.a out of the sysroot.
[ -x "$PREFIX/bin/x86_64-prismos-gcc" ] || {
        echo "ports/binutils: no toolchain in $PREFIX -- run \`make mlibc\` first" >&2
        exit 1
}
[ -f "$SYSROOT/usr/lib/libc.a" ] || {
        echo "ports/binutils: no libc.a in the sysroot -- run \`make mlibc\` first" >&2
        exit 1
}

# ---- stage: fetch -------------------------------------------------------------
# The same tarball and the same triplet patch ports/toolchain fetches for
# the cross tools: nothing is downloaded twice, and the sources know the
# x86_64-prismos triplet exactly the way build.sh left them.
do_fetch() {
        mkdir -p "$SRC"
        cd "$SRC"
        f=binutils-$BINUTILS_VER.tar.xz
        if [ ! -f "$f" ]; then
                say fetch "$f"
                curl -sS --retry 3 -C - -o "$f.part" "$MIRROR/binutils/$f" && mv "$f.part" "$f"
        fi
        if [ ! -d "binutils-$BINUTILS_VER" ]; then
                say fetch "extracting $f"
                tar xf "$f"
        fi
        patch -d "$SRC/binutils-$BINUTILS_VER" -p1 -N --no-backup-if-mismatch \
                < "$PATCHES/binutils-$BINUTILS_VER-prismos.patch" \
                || [ $? -eq 1 ]   # already applied
        say fetch "sources ready (binutils-$BINUTILS_VER, patched for x86_64-prismos)"
}

# ---- stage: cc ----------------------------------------------------------------
# prism-cc, the driver configure and make will call "the compiler": a
# compile goes through x86_64-prismos-gcc with the do_prog flag set (the
# mlibc sysroot's includes, freestanding, -fPIE -- the same command
# tools/prism.py builds every app with), a link assembles the static-PIE
# sandwich the kernel's loader expects (Scrt1 / crti / crtbeginS / the
# objects / libc.a / crtendS / crtn, -z separate-code -z max-page-size=4096).
# The driver resolves its own paths at run time, so it works unchanged
# behind the bootstrap wrappers (the fast path) or build.sh's real cross
# tools, and it survives the toolchain being rebuilt under it.
do_cc() {
        mkdir -p "$WORK"
        cat > "$WORK/prism-cc" <<'EOF'
#!/bin/sh
# prism-cc -- the compiler driver of phase 4.1 (generated by
# ports/binutils/build.sh): binutils' configure and make talk to it as CC,
# and it answers with PrismOS objects and static PIEs.
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
# configure's probes hand the driver a .c (compile and link in one
# command); make's link rules hand it objects and archives.  Both end in
# the same sandwich.
link() {
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
        # a compiler's default output name, the way configure's "whether the
        # C compiler works" probe uses it (no -o in the command)
        [ -n "$out" ] || out=a.out

        # the one-command compile-and-link of configure's probes
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

        # -lfoo resolves against the -L list (the build's own directories:
        # libz from the in-tree zlib, nothing else should appear)
        resolved=
        for l in $libs; do
                found=
                for d in $ldirs; do
                        if [ -f "$d/lib$l.a" ]; then found="$d/lib$l.a"; break; fi
                done
                [ -n "$found" ] || { echo "prism-cc: cannot resolve -l$l" >&2; exit 1; }
                resolved="$resolved $found"
        done

        # libc.a leads the group: the utilities ask the C library for
        # getopt_long (and fnmatch, getcwd, ...) before libiberty.a -- whose
        # getopt.o / fnmatch.o are REQUIRED_OFILES, always in the archive --
        # is ever scanned.  A shared libc never sees the conflict (no member
        # extraction); a static one resolves it by order, the group keeping
        # the archives' mutual dependencies satisfiable in both directions.
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
        say cc "$WORK/prism-cc (compile: $PREFIX/bin/x86_64-prismos-gcc, link: the static-PIE sandwich)"
}

# ---- stage: configure ---------------------------------------------------------
# host = target = x86_64-prismos, build = whatever runs this script: a
# plain cross compile of the utilities.  gas and ld are not this phase's
# (--without-gnu-as / --without-gnu-ld take them off the build list; `as`
# and `ld` are phase 5.1's "native binutils"), gdb and friends are off the
# way ports/toolchain/build.sh already does it, and the libraries the five
# tools need -- libiberty, bfd, opcodes, libsframe, the in-tree zlib -- are
# archived by the cross AR / RANLIB ("used by the builds").
do_configure() {
        [ -d "$SRC/binutils-$BINUTILS_VER" ] || do_fetch
        [ -x "$WORK/prism-cc" ] || do_cc
        rm -rf "$WORK/obj"
        mkdir -p "$WORK/obj"
        cd "$WORK/obj"
        say configure "--host=x86_64-prismos --target=x86_64-prismos"
        BUILD=$(sh "$SRC/binutils-$BINUTILS_VER/config.guess")
        env CC="$WORK/prism-cc" \
            AR="$PREFIX/bin/x86_64-prismos-ar" \
            RANLIB="$PREFIX/bin/x86_64-prismos-ranlib" \
            STRIP="$PREFIX/bin/x86_64-prismos-strip" \
            CFLAGS="-O2" \
            LDFLAGS="" \
        "$SRC/binutils-$BINUTILS_VER/configure" \
                --build="$BUILD" \
                --host=x86_64-prismos \
                --target=x86_64-prismos \
                --prefix=/usr \
                --disable-nls --disable-werror --disable-shared \
                --disable-gdb --disable-gdbserver --disable-gprofng \
                --disable-gold --disable-libctf --disable-sim \
                --without-gnu-as --without-gnu-ld \
                --enable-deterministic-archives \
                > "$WORK/configure.log" 2>&1 \
                || { tail -30 "$WORK/configure.log"; exit 1; }
        say configure "done (log: $WORK/configure.log)"
}

# ---- stage: build --------------------------------------------------------------
# Only the utilities' subtree: all-binutils pulls the libraries it needs
# (libiberty, bfd, opcodes, libsframe, zlib) through the top-level's own
# dependencies, and leaves as and ld -- not configured at all -- out.
do_build() {
        [ -x "$WORK/prism-cc" ] || do_cc
        [ -f "$WORK/obj/Makefile" ] || do_configure
        cd "$WORK/obj"
        say build "make -j$JOBS all-binutils (ar, ranlib, nm-new, objdump, strip-new...)"
        make -j"$JOBS" all-binutils > "$WORK/build.log" 2>&1 \
                || { tail -40 "$WORK/build.log"; exit 1; }
        for t in ar ranlib $BUILD_NAME_NM $BUILD_NAME_STRIP objdump; do
                [ -f "binutils/$t" ] || { echo "ports/binutils: binutils/$t missing" >&2; exit 1; }
        done
        say build "the utilities are PrismOS binaries (binutils/ in $WORK/obj)"
}

# ---- stage: stage --------------------------------------------------------------
# The five, under their image names, stripped with the cross strip (the
# kernel reads whole files: MAX_ELF_FILE is 4 MiB) into build/binutils-out.
do_stage() {
        do_build
        mkdir -p "$OUT"
        STRIP=$PREFIX/bin/x86_64-prismos-strip
        for t in $TOOLS; do
                bt=$t
                [ "$t" = nm ] && bt=$BUILD_NAME_NM
                [ "$t" = strip ] && bt=$BUILD_NAME_STRIP
                say stage "stripping $t"
                $STRIP --strip-all -o "$OUT/$t" "$WORK/obj/binutils/$bt"
        done
        say stage "$OUT: $(ls "$OUT" | tr '\n' ' ')"
}

# ---- stage: verify -------------------------------------------------------------
# The loader's own checks on each of the five (verify.sh's custom mode), on
# the unstripped binary -- the symbols the diet checks live in, exactly the
# way tools/prism.py verifies an app before staging its stripped half.
# The diet each tool actually speaks, traced under prism_emu (-v,
# tests/binutils_test.py drives the same scenarios): write / read / open /
# close / lseek, the stat pair and lstat, mmap (mlibc's allocator) with
# munmap when it frees, fcntl for the close-on-exec mark, ioctl's isatty
# probe, getpid and arch_prctl from the library's own startup, dup / unlink
# for the archive and rename dance of ar / ranlib / strip.  The symbols:
# the bfd call every tool opens its files through, and getopt_long -- the
# GNU long-option entry phase 4.1 added the glibc option to mlibc for.
do_verify() {
        [ -f "$WORK/obj/binutils/ar" ] || do_build
        diet_ar="1 2 13 14 15 16 17 19 20 22 23 27 29 30 36 58"
        diet_ranlib="1 2 13 14 15 16 17 19 20 22 23 27 29 30 36 58"
        diet_nm="1 2 13 14 15 16 19 20 22 27 29 36 58"
        diet_objdump="1 2 13 14 15 16 19 20 22 23 27 29 36"
        diet_strip="1 2 13 14 15 16 19 20 22 27 29 30 36 58"
        for t in $TOOLS; do
                bt=$t
                [ "$t" = nm ] && bt=$BUILD_NAME_NM
                [ "$t" = strip ] && bt=$BUILD_NAME_STRIP
                eval diet="\$diet_$t"
                say verify "verify.sh $t (loader contract + syscall diet)"
                PRISM_VERIFY_SYSCALLS="$diet" \
                PRISM_VERIFY_SYMBOLS="bfd_openr getopt_long" \
                sh "$REPO/ports/toolchain/verify.sh" \
                        "$WORK/obj/binutils/$bt" "$SYSROOT" custom
        done
}

case "$STAGE" in
        all)      do_fetch; do_cc; do_configure; do_build; do_stage; do_verify ;;
        fetch)    do_fetch ;;
        cc)       do_cc ;;
        configure) do_configure ;;
        build)    do_build ;;
        stage)    do_stage ;;
        verify)   do_verify ;;
        *) echo "unknown stage: $STAGE (try: list)" >&2; exit 2 ;;
esac
