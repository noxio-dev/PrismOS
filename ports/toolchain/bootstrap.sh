#!/bin/sh
#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# Phase 3.2, steps 3 + 4 -- the mlibc build against a x86_64-prismos cross
# compiler, installed into a sysroot ("build mlibc with Meson against a cross
# C/C++ compiler ... install it into a sysroot"), and the four /bin programs
# ported off the mini libc onto it ("port /bin/hello, /bin/fork, /bin/fd and
# /bin/posix from the mini libc (usr/lib) to mlibc").
#
# The bootstrap path: the cross compiler is a *prefixed pass-through* around
# any C23/C++23-capable host compiler -- exactly the arrangement the step-2
# verification used (meson 1.12, gcc 14), because mlibc isolates its own
# includes (-nostdinc for the C++ sources, -ffreestanding, its internal
# include dirs first) and the cross file is what makes the build *PrismOS*
# rather than the host system.  ports/toolchain/build.sh replaces the
# wrappers with the real binutils + GCC cross tools under the same names,
# then rebuilds this very pipeline; nothing else changes.
#
# The order is the one the phase's todo spells out -- headers first, then the
# compiler, then the library:
#   stage wrappers  the x86_64-prismos-* bootstrap compilers
#   stage mlibc     the mlibc checkout (clone + patch + sysdeps, once)
#   stage headers   mlibc's headers, installed into the sysroot
#   stage library   mlibc proper: libc.a + the four crt objects, static,
#                   the whole ansi + posix surface the kernel serves
#   stage progs     the four /bin programs ported to mlibc (usr/mlibc):
#                   compiled with the cross compiler, linked as the static
#                   PIEs the loader expects (step 4)
#   stage hello     the step-3 proof (ports/toolchain/hello.c), kept as the
#                   regression it was; `stage prog <name>` builds one port
#   stage verify    the loader's own checks, statically (verify.sh), for
#                   each of the four ports
#
# Usage:
#   sh ports/toolchain/bootstrap.sh                 # all stages, in order
#   sh ports/toolchain/bootstrap.sh stage <name>    # one stage
#   sh ports/toolchain/bootstrap.sh stage prog <n>  # one program
#   sh ports/toolchain/bootstrap.sh list            # the stage names
#
# Environment / flags:
#   --mlibc <dir>   prepared mlibc checkout (default build/mlibc)
#   --prefix <dir>  toolchain prefix (default build/toolchain/usr)
#   --jobs <n>      parallel jobs (default: nproc, capped at 8)
#   MLIBC_REF       the mlibc tag to build (default v7.0.0, the release the
#                   port is verified against)
#
# What lands where:
#   $PREFIX/bin/x86_64-prismos-{gcc,g++,ar,...}   the bootstrap compilers
#   $PREFIX/x86_64-prismos/usr/include/           mlibc's headers
#   $PREFIX/x86_64-prismos/usr/lib/{libc.a,crt1.o,Scrt1.o,crti.o,crtn.o}
#   build/usr/bin-mlibc/{hello,fork,fd,posix}     the ports, static PIEs
set -e

REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=$REPO/build/toolchain/usr
SYSROOT=$PREFIX/x86_64-prismos
MLIBC=$REPO/build/mlibc
MLIBC_REF=${MLIBC_REF:-v7.0.0}
JOBS=$(nproc 2>/dev/null || echo 2)
[ "$JOBS" -gt 8 ] && JOBS=8
STAGE=all

while [ $# -gt 0 ]; do
        case "$1" in
        --mlibc)  MLIBC=$2; shift 2 ;;
        --prefix) PREFIX=$2; SYSROOT=$2/x86_64-prismos; shift 2 ;;
        --jobs)   JOBS=$2; shift 2 ;;
        stage)    STAGE=$2
                  if [ "$2" = prog ] && [ $# -ge 3 ]; then PROG=$3; shift 1; fi
                  shift 2 ;;
        list)     echo "wrappers mlibc headers library progs hello verify"; exit 0 ;;
        *) echo "usage: $0 [--mlibc dir] [--prefix dir] [--jobs n] [stage <name>|list]" >&2; exit 2 ;;
        esac
done
export PATH="$PREFIX/bin:$PATH"

say() { printf '[bootstrap/%s] %s\n' "$1" "$2"; }

# ---- stage: wrappers ---------------------------------------------------------
# The bootstrap compilers: prefixed pass-throughs to the host tools.  They are
# replaced (never duplicated) by the real cross tools -- build.sh installs
# gcc/binutils binaries under the same names, and this stage skips anything
# that is not a wrapper anymore.
do_wrappers() {
        mkdir -p "$PREFIX/bin"
        # Host compilers behind the wrappers.  Prefer clang: GCC >= 16 rejects the
        # evex512 target attribute used by mlibc's freestnd-c-hdrs.  Override with
        # HOST_CC / HOST_CXX.
        if [ -z "${HOST_CC:-}" ]; then
                if command -v clang >/dev/null 2>&1; then HOST_CC=clang; else HOST_CC=gcc; fi
        fi
        if [ -z "${HOST_CXX:-}" ]; then
                if command -v clang++ >/dev/null 2>&1; then HOST_CXX=clang++; else HOST_CXX=g++; fi
        fi
        for t in gcc g++ cc c++ ar ranlib nm strip ld as objcopy; do
                case $t in
                gcc|cc)  tool=$HOST_CC ;;
                g++|c++) tool=$HOST_CXX ;;
                *)       tool=$t ;;
                esac
                dst=$PREFIX/bin/x86_64-prismos-$t
                # a file that is not one of our wrappers (build.sh's real cross
                # tools) is left alone; the marker is on line 2 of a wrapper
                [ -f "$dst" ] && ! head -2 "$dst" | grep -q 'bootstrap wrapper' \
                        && continue
                cat > "$dst" <<EOF
#!/bin/sh
# bootstrap wrapper -- phase 3.2, step 3 (ports/toolchain/bootstrap.sh)
# Pass-through to the host tool; the cross file and the sysroot are what make
# the build PrismOS.  build.sh's real x86_64-prismos tools replace this file.
exec $tool "\$@"
EOF
                chmod +x "$dst"
        done
        say wrappers "x86_64-prismos-{gcc,g++,ar,...} bootstrap compilers in $PREFIX/bin (cc=$HOST_CC c++=$HOST_CXX)"
}

# ---- stage: mlibc ------------------------------------------------------------
# The mlibc checkout the port applies to: clone once, pin the release, apply
# the registration patch, drop the sysdeps in.
do_mlibc() {
        if [ ! -d "$MLIBC" ]; then
                say mlibc "cloning mlibc at $MLIBC_REF"
                git clone https://github.com/managarm/mlibc "$MLIBC"
        fi
        (cd "$MLIBC"
         git checkout -q "$MLIBC_REF"
         if ! git apply --check "$REPO/ports/mlibc/mlibc-prismos.patch" 2>/dev/null; then
                say mlibc "patch already applied"
         else
                git apply "$REPO/ports/mlibc/mlibc-prismos.patch"
                say mlibc "registration patch applied"
         fi
         rm -rf sysdeps/prismos
         cp -r "$REPO/ports/mlibc/sysdeps/prismos" sysdeps/
         say mlibc "sysdeps/prismos in place")
}

# ---- stage: headers ----------------------------------------------------------
# "Headers first": mlibc's headers alone, installed into the sysroot the
# cross compiler of build.sh will build against (-Dheaders_only skips the
# library and the crt objects).
do_headers() {
        do_mlibc
        rm -rf "$REPO/build/mlibc-headers"
        say headers "meson setup (headers_only)"
        meson setup "$REPO/build/mlibc-headers" "$MLIBC" \
                --cross-file "$REPO/ports/toolchain/cross.txt" \
                --prefix=/usr \
                -Dheaders_only=true -Ddefault_library=static -Dbuild_tests=false \
                >/dev/null
        ninja -C "$REPO/build/mlibc-headers"
        meson install -C "$REPO/build/mlibc-headers" --destdir "$SYSROOT" >/dev/null
        say headers "sysroot headers in $SYSROOT/usr/include"
}

# ---- stage: library ----------------------------------------------------------
# "Then the library": mlibc proper, static (dynamic linking is the phase's
# last step), the whole ansi + posix surface the kernel serves, plus the four
# crt objects the port's meson.build installs.
do_library() {
        do_mlibc
        rm -rf "$REPO/build/mlibc-full"
        say library "meson setup (ansi + posix, static)"
        meson setup "$REPO/build/mlibc-full" "$MLIBC" \
                --cross-file "$REPO/ports/toolchain/cross.txt" \
                --prefix=/usr \
                -Ddefault_library=static -Dbuild_tests=false \
                >/dev/null
        ninja -C "$REPO/build/mlibc-full" -j "$JOBS"
        meson install -C "$REPO/build/mlibc-full" --destdir "$SYSROOT" >/dev/null
        say library "libc.a + crt objects in $SYSROOT/usr/lib"
}

# ---- stage: hello ------------------------------------------------------------
# /bin/hello rebuilt against mlibc.  The compile carries the sysroot's
# includes explicitly (-nostdinc + the compiler's own headers + the sysroot),
# the same arrangement a real cross gcc provides through --with-sysroot; the
# link is the loader's contract: static PIE, page-aligned segments, no
# interpreter, DT_RELA for the RELATIVE relocations.
do_hello() {
        CC=$PREFIX/bin/x86_64-prismos-gcc
        LD=$PREFIX/bin/x86_64-prismos-ld
        GCCINC=$($CC -print-file-name=include)
        # Its own directory: the proof and the step-4 port share a name, and
        # one overwriting the other made every regression run order-sensitive.
        OUTDIR=$REPO/build/usr/hello-mlibc
        mkdir -p "$OUTDIR"

        say hello "compiling ports/toolchain/hello.c"
        $CC -c "$REPO/ports/toolchain/hello.c" -o "$OUTDIR/hello.o" \
                -O2 -Wall -Wextra -ffreestanding -fPIE \
                -fno-stack-protector -fno-asynchronous-unwind-tables -fno-math-errno \
                -nostdinc -isystem "$GCCINC" -isystem "$SYSROOT/usr/include"

        say hello "linking the static PIE"
        # The sandwich a -static-pie compiler driver assembles: Scrt1 (the
        # PIE-aware _start) first, crti/crtbegin around the program and the
        # library (crtbeginS.o is what defines __dso_handle -- the mlibc
        # constructors register their destructors against it), crtn last.
        # -print-file-name finds the pair for this compiler: the host gcc's while
        # the wrappers bootstrap, the cross gcc's own once build.sh installed it.
        CRTBEGIN=$($CC -print-file-name=crtbeginS.o)
        CRTEND=$($CC -print-file-name=crtendS.o)
        if ! $LD -m elf_x86_64 -pie --no-dynamic-linker -z separate-code \
              -z max-page-size=4096 -z noexecstack -z norelro -e _start \
              -o "$OUTDIR/hello" \
              "$SYSROOT/usr/lib/Scrt1.o" "$SYSROOT/usr/lib/crti.o" \
              "$CRTBEGIN" "$OUTDIR/hello.o" \
              --start-group "$SYSROOT/usr/lib/libc.a" --end-group \
              "$CRTEND" "$SYSROOT/usr/lib/crtn.o" 2>/dev/null; then
                say hello "retrying with libgcc (compiler intrinsics)"
                LIBGCC=$($CC -print-libgcc-file-name)
                $LD -m elf_x86_64 -pie --no-dynamic-linker -z separate-code \
                      -z max-page-size=4096 -z noexecstack -z norelro -e _start \
                      -o "$OUTDIR/hello" \
                      "$SYSROOT/usr/lib/Scrt1.o" "$SYSROOT/usr/lib/crti.o" \
                      "$CRTBEGIN" "$OUTDIR/hello.o" \
                      --start-group "$SYSROOT/usr/lib/libc.a" "$LIBGCC" --end-group \
                      "$CRTEND" "$SYSROOT/usr/lib/crtn.o"
        fi
        say hello "build/usr/hello-mlibc/hello"
}

# ---- stage: progs ------------------------------------------------------------
# The step-4 port: the five /bin programs off the mini libc, onto mlibc
# (usr/mlibc).  Same discipline as the step-3 proof -- the sysroot's headers
# carried explicitly, freestanding compile, the loader's link contract -- only
# the sources change, and they pull the whole `ansi` + `posix` surface in:
# stdio buffered on the descriptor table, fork / waitpid / kill, opendir,
# tcgetattr, uname, ...  The PrismOS-only calls (uptime_ms, yield, the
# millisecond sleep, brk, and the cube's framebuffer pair) stay raw, through
# usr/mlibc/prism_raw.h -- the only place the `syscall` instruction appears
# outside libc.a.
do_prog() { # <name>
        name=$1
        CC=$PREFIX/bin/x86_64-prismos-gcc
        LD=$PREFIX/bin/x86_64-prismos-ld
        GCCINC=$($CC -print-file-name=include)
        OUTDIR=$REPO/build/usr/bin-mlibc
        mkdir -p "$OUTDIR"

        say "$name" "compiling usr/mlibc/$name/$name.c"
        $CC -c "$REPO/usr/mlibc/$name/$name.c" -o "$OUTDIR/$name.o" \
                -O2 -Wall -Wextra -ffreestanding -fPIE \
                -fno-stack-protector -fno-asynchronous-unwind-tables -fno-math-errno \
                -nostdinc -isystem "$GCCINC" -isystem "$SYSROOT/usr/include"

        say "$name" "linking the static PIE"
        CRTBEGIN=$($CC -print-file-name=crtbeginS.o)
        CRTEND=$($CC -print-file-name=crtendS.o)
        if ! $LD -m elf_x86_64 -pie --no-dynamic-linker -z separate-code \
              -z max-page-size=4096 -z noexecstack -z norelro -e _start \
              -o "$OUTDIR/$name" \
              "$SYSROOT/usr/lib/Scrt1.o" "$SYSROOT/usr/lib/crti.o" \
              "$CRTBEGIN" "$OUTDIR/$name.o" \
              --start-group "$SYSROOT/usr/lib/libc.a" --end-group \
              "$CRTEND" "$SYSROOT/usr/lib/crtn.o" 2>/dev/null; then
                say "$name" "retrying with libgcc (compiler intrinsics)"
                LIBGCC=$($CC -print-libgcc-file-name)
                $LD -m elf_x86_64 -pie --no-dynamic-linker -z separate-code \
                      -z max-page-size=4096 -z noexecstack -z norelro -e _start \
                      -o "$OUTDIR/$name" \
                      "$SYSROOT/usr/lib/Scrt1.o" "$SYSROOT/usr/lib/crti.o" \
                      "$CRTBEGIN" "$OUTDIR/$name.o" \
                      --start-group "$SYSROOT/usr/lib/libc.a" "$LIBGCC" --end-group \
                      "$CRTEND" "$SYSROOT/usr/lib/crtn.o"
        fi
        say "$name" "build/usr/bin-mlibc/$name"
}

do_progs() {
        for p in hello fork fd posix cube; do
                do_prog "$p"
        done
}

# ---- stage: verify -----------------------------------------------------------
# The loader's own checks (verify.sh), statically, for each of the four ports
# with its own syscall and symbol set.  (The step-3 proof of `stage hello` is
# verified with the no-program call: verify.sh <elf> <sysroot>.)
do_verify() {
        for p in hello fork fd posix cube; do
                echo "--- $p"
                sh "$REPO/ports/toolchain/verify.sh" "$REPO/build/usr/bin-mlibc/$p" "$SYSROOT" "$p"
        done
}

case "$STAGE" in
        all)     do_wrappers; do_mlibc; do_headers; do_library; do_progs; do_verify ;;
        wrappers) do_wrappers ;;
        mlibc)    do_mlibc ;;
        headers)  do_headers ;;
        library)  do_library ;;
        progs)    do_progs ;;
        prog)     [ -n "$PROG" ] || { echo "stage prog: name required (hello | fork | fd | posix | cube)" >&2; exit 2; }
                  do_prog "$PROG" ;;
        hello)    do_hello ;;
        verify)   do_verify ;;
        *) echo "unknown stage: $STAGE (try: list)" >&2; exit 2 ;;
esac
