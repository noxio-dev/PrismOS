#!/bin/sh
#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# The nano port: GNU nano 9.3 on ncurses 6.6, cross-built for
# x86_64-prismos and installed into the image -- a full-screen editor on
# the console the ANSI interpreter (sys/kern/src/console/ansi.rs) made
# into a terminal.
#
# Nano cannot draw by itself: the curses layer is part of the port.
# ncurses 6.6 builds first as one static library (libncursesw for the
# wide/UTF-8 API -- with_termlib defaults to no in 6.6, so the terminfo
# core, tgetent and setupterm included, is archived inside it), -- configured
# --without-progs so no tic / tput / toe is built for the target, the
# image's terminfo comes from the HOST tic compiling ports/nano/prism.src
# (the TERM=prism entry, a faithful description of console/ansi.rs) into
# build/nano-out/terminfo -- then nano itself, configured against the
# ncurses the port installed into its own sysroot (build/nano-native/
# sysroot: the mlibc sysroot stays pristine, the way the toolchain's
# rebuild would wipe a stranger's headers).
#
# The ncurses side of the port is what makes accents work: mlibc's
# mbstowcs / wcrtomb decode UTF-8 unconditionally (the ports/mlibc
# decision), ncursesw counts screen columns per wide character --
# exactly what the console draws (one glyph per character, unifont) --
# and the terminfo's am-without-xenl matches the DECAWM wrap-pending
# discipline both consoles implement.
#
# One binary comes out, stripped under the kernel's 4 MiB whole-file
# read, into build/nano-out/nano; the Makefile's root rule copies it and
# the terminfo into the image.
#
# Usage:
#   sh ports/nano/build.sh               # every stage, in order
#   sh ports/nano/build.sh stage <name>  # one stage
#   sh ports/nano/build.sh list
#
# Environment / flags:
#   --prefix <dir>    toolchain prefix (default build/toolchain/usr)
#   --src <dir>       sources directory (default build/nano-src)
#   --jobs <n>        parallel jobs (default nproc, capped at 8)
#   NCURSES_VER       default 6.6
#   NANO_VER          default 9.3
#   GNU               default https://mirrors.kernel.org/gnu
#
# Requirements: whatever `make mlibc` already needed, plus the host tic
# (ncurses' terminfo compiler -- Ubuntu's ncurses-bin, Debian's too) and
# the two tarballs (stage fetch).  The binary is verified against the
# loader's contract by ports/toolchain/verify.sh, and `nano --version`
# is run under prism_emu (the repo's ptrace runner, what the binutils
# and busybox tests drive) as the functional smoke.
set -e

REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=$REPO/build/toolchain/usr
SYSROOT=$PREFIX/x86_64-prismos
SRC=$REPO/build/nano-src
WORK=$REPO/build/nano-native
OUT=$REPO/build/nano-out
JOBS=$(nproc 2>/dev/null || echo 2)
[ "$JOBS" -gt 8 ] && JOBS=8
NCURSES_VER=${NCURSES_VER:-6.6}
NANO_VER=${NANO_VER:-9.3}
GNU=${GNU:-https://mirrors.kernel.org/gnu}
# The curses entry point TERM=prism looks up at runtime, in ncurses'
# layout: /usr/share/terminfo/<first letter>/<name>.
TERMINFO_FILE=p/prism
STAGE=all

while [ $# -gt 0 ]; do
        case "$1" in
        --prefix) PREFIX=$2; SYSROOT=$2/x86_64-prismos; shift 2 ;;
        --src)    SRC=$2; shift 2 ;;
        --jobs)   JOBS=$2; shift 2 ;;
        stage)    STAGE=$2; shift 2 ;;
        list)     echo "fetch cc curses nano stage verify"; exit 0 ;;
        *) echo "usage: $0 [--prefix dir] [--src dir] [--jobs n] [stage <name>|list]" >&2; exit 2 ;;
        esac
done

PATCHES=$REPO/ports/nano/patches
say() { printf '[nano/%s] %s\n' "$1" "$2"; }

# The toolchain has to be there first: the driver compiles through
# x86_64-prismos-gcc and the sandwich links libc.a out of the sysroot.
[ -x "$PREFIX/bin/x86_64-prismos-gcc" ] || {
        echo "ports/nano: no toolchain in $PREFIX -- run \`make mlibc\` first" >&2
        exit 1
}
[ -f "$SYSROOT/usr/lib/libc.a" ] || {
        echo "ports/nano: no libc.a in the sysroot -- run \`make mlibc\` first" >&2
        exit 1
}
# The terminfo is compiled ONCE, by the host's own ncurses compiler: the
# on-disk format is stable across releases, the target builds no progs.
command -v tic >/dev/null || {
        echo "ports/nano: no host tic (the ncurses terminfo compiler) -- install ncurses-bin" >&2
        exit 1
}

# ---- stage: fetch -------------------------------------------------------------
# ncurses 6.6 and nano 9.3, tarballs from the GNU mirror (the binutils
# port's habit), each config.sub taught the x86_64-prismos triplet the
# way ports/toolchain taught binutils and gcc.
do_fetch() {
        mkdir -p "$SRC"
        cd "$SRC"
        for pair in "ncurses/$NCURSES_VER ncurses-$NCURSES_VER.tar.gz" \
                    "nano/$NANO_VER nano-$NANO_VER.tar.xz"; do
                path=${pair%% *}; f=${pair##* }
                if [ ! -f "$f" ]; then
                        say fetch "$f"
                        curl -sS --retry 3 -C - -o "$f.part" "$GNU/$path" && mv "$f.part" "$f"
                fi
                d=${f%.tar.gz}; d=${d%.tar.xz}
                if [ ! -d "$d" ]; then
                        say fetch "extracting $f"
                        case $f in *.tar.gz) tar xzf "$f" ;; *) tar xf "$f" ;; esac
                fi
                if patch -d "$d" -p1 -N --no-backup-if-mismatch \
                                < "$PATCHES/$d-prismos.patch" >/dev/null 2>&1; then
                        say fetch "$d: prismos triplet patch applied"
                else
                        say fetch "$d: prismos triplet patch already in place"
                fi
        done
        say fetch "sources ready (ncurses-$NCURSES_VER, nano-$NANO_VER)"
}

# ---- stage: cc ----------------------------------------------------------------
# prism-cc, the driver configure and make will call "the compiler" -- the
# binutils port's driver, with one answer added: an -l that resolves
# nowhere in the -L list falls back to the mlibc sysroot's own archives
# (libm above all -- mlibc ships one, and ncurses' and nano's probes
# spell -lm with no -L).  The static-PIE sandwich and the do_prog compile
# discipline are exactly ports/binutils'.
do_cc() {
        mkdir -p "$WORK"
        cat > "$WORK/prism-cc" <<'EOF'
#!/bin/sh
# prism-cc -- the compiler driver of the nano port (generated by
# ports/nano/build.sh): ncurses' and nano's configure and make talk to it
# as CC, and it answers with PrismOS objects and static PIEs.
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
        # the ncurses archives, nothing else should appear), then against
        # the mlibc sysroot's own (libm -- mlibc ships one, and the probes
        # spell -lm with no -L to carry it)
        resolved=
        for l in $libs; do
                found=
                for d in $ldirs "$SYSROOT/usr/lib"; do
                        if [ -f "$d/lib$l.a" ]; then found="$d/lib$l.a"; break; fi
                done
                [ -n "$found" ] || { echo "prism-cc: cannot resolve -l$l" >&2; exit 1; }
                resolved="$resolved $found"
        done

        # libc.a leads the group: the programs ask the C library for
        # getopt_long / regcomp / the math before anything else is
        # scanned, and the group keeps the archives' mutual dependencies
        # satisfiable in both directions (ncursesw needs tinfo's symbols,
        # whatever order configure picked).
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

# ---- stage: curses --------------------------------------------------------------
# ncurses 6.6 for the target: libncursesw.a (the wide-character API nano
# links) and libtinfo.a (the terminfo core), static, --without-progs (no
# target tic / tput / toe: the image gets its terminfo from the host tic
# below) and --disable-db-install (the entry travels with the port).
# The libraries and headers install into the port's own sysroot --
# build/nano-native/sysroot -- so the mlibc sysroot stays pristine.
do_curses() {
        [ -d "$SRC/ncurses-$NCURSES_VER" ] || do_fetch
        [ -x "$WORK/prism-cc" ] || do_cc
        rm -rf "$WORK/ncurses-obj"
        mkdir -p "$WORK/ncurses-obj"
        cd "$WORK/ncurses-obj"
        say curses "configure --host=x86_64-prismos (widec, no progs, no db)"
        # _GNU_SOURCE: mlibc's search.h keeps tsearch / tdelete behind it,
        # and new_pair.c calls them with no include of its own (upstream
        # rides the ext-colors gate; gcc's implicit declaration is a
        # warning there, clang's int-to-pointer conversions are errors).
        BUILD=$(sh "$SRC/ncurses-$NCURSES_VER/config.guess")
        BUILD_CC=$(command -v cc || command -v clang)
        env CC="$WORK/prism-cc" \
            AR="$PREFIX/bin/x86_64-prismos-ar" \
            RANLIB="$PREFIX/bin/x86_64-prismos-ranlib" \
            STRIP="$PREFIX/bin/x86_64-prismos-strip" \
            CFLAGS="-O2 -D_GNU_SOURCE" \
            LDFLAGS="" \
        "$SRC/ncurses-$NCURSES_VER/configure" \
                --build="$BUILD" \
                --host=x86_64-prismos \
                --prefix=/usr \
                --enable-widec \
                --without-cxx --without-cxx-binding \
                --without-progs --without-tack --without-tests \
                --without-manpages \
                --without-shared --with-normal \
                --without-gpm --without-dlsym \
                --disable-db-install \
                --with-terminfo-dirs=/usr/share/terminfo \
                --with-default-terminfo-dir=/usr/share/terminfo \
                --with-build-cc="$BUILD_CC" --with-build-cflags="-O2" \
                > "$WORK/ncurses-configure.log" 2>&1 \
                || { tail -30 "$WORK/ncurses-configure.log"; exit 1; }
        say curses "make -j$JOBS (libncursesw.a)"
        make -j"$JOBS" > "$WORK/ncurses-build.log" 2>&1 \
                || { tail -40 "$WORK/ncurses-build.log"; exit 1; }
        for lib in libncursesw.a; do
                [ -f "lib/$lib" ] || { echo "ports/nano: lib/$lib missing" >&2; exit 1; }
        done
        say curses "install into the port's sysroot"
        make -j1 install.libs DESTDIR="$WORK/sysroot" \
                > "$WORK/ncurses-install.log" 2>&1 \
                || { tail -20 "$WORK/ncurses-install.log"; exit 1; }
        [ -f "$WORK/sysroot/usr/lib/libncursesw.a" ] \
                || { echo "ports/nano: libncursesw.a not installed" >&2; exit 1; }
        say curses "$WORK/sysroot/usr/lib: libncursesw.a (terminfo core inside), headers in usr/include"
}

# ---- stage: nano ------------------------------------------------------------------
# nano 9.3 against the port's ncurses: the header and library paths ride
# CFLAGS / LDFLAGS (the driver passes -I through on compiles, resolves
# the -l list at link), PKG_CONFIG is a deliberate false so configure's
# ncursesw probe cannot ask the HOST's ncurses, and -lncursesw carries
# everything -- curses and the terminfo core alike (see stage curses).
do_nano() {
        [ -f "$WORK/sysroot/usr/lib/libncursesw.a" ] || do_curses
        [ -d "$SRC/nano-$NANO_VER" ] || do_fetch
        [ -x "$WORK/prism-cc" ] || do_cc
        rm -rf "$WORK/nano-obj"
        mkdir -p "$WORK/nano-obj"
        cd "$WORK/nano-obj"
        say nano "configure --host=x86_64-prismos (the port's ncursesw, no NLS, no speller)"
        BUILD=$(sh "$SRC/nano-$NANO_VER/config.guess")
        env CC="$WORK/prism-cc" \
            AR="$PREFIX/bin/x86_64-prismos-ar" \
            RANLIB="$PREFIX/bin/x86_64-prismos-ranlib" \
            STRIP="$PREFIX/bin/x86_64-prismos-strip" \
            PKG_CONFIG=false \
            CFLAGS="-O2 -I$WORK/sysroot/usr/include" \
            LDFLAGS="-L$WORK/sysroot/usr/lib" \
            LIBS="-lncursesw" \
        "$SRC/nano-$NANO_VER/configure" \
                --build="$BUILD" \
                --host=x86_64-prismos \
                --prefix=/usr \
                --disable-nls --disable-rpath \
                --disable-speller \
                > "$WORK/nano-configure.log" 2>&1 \
                || { tail -30 "$WORK/nano-configure.log"; exit 1; }
        say nano "make -j$JOBS (src/nano)"
        make -j"$JOBS" > "$WORK/nano-build.log" 2>&1 \
                || { tail -40 "$WORK/nano-build.log"; exit 1; }
        [ -f "src/nano" ] || { echo "ports/nano: src/nano missing" >&2; exit 1; }
        say nano "src/nano built (static PIE, ncursesw inside)"
}

# ---- stage: stage -------------------------------------------------------------------
# The editor under the kernel's 4 MiB whole-file read (the cross strip
# does it) and the terminfo entry, both into build/nano-out.
do_stage() {
        [ -f "$WORK/nano-obj/src/nano" ] || do_nano
        mkdir -p "$OUT"
        say stage "stripping nano"
        $PREFIX/bin/x86_64-prismos-strip --strip-all -o "$OUT/nano" "$WORK/nano-obj/src/nano"
        say stage "the terminfo (host tic -> $TERMINFO_FILE)"
        rm -rf "$OUT/terminfo"
        tic -o "$OUT/terminfo" "$REPO/ports/nano/prism.src" > "$WORK/tic.log" 2>&1 \
                || { cat "$WORK/tic.log"; exit 1; }
        [ -f "$OUT/terminfo/$TERMINFO_FILE" ] \
                || { echo "ports/nano: terminfo/$TERMINFO_FILE missing" >&2; exit 1; }
        say stage "$OUT: $(ls "$OUT" | tr '\n' ' ')"
}

# ---- stage: verify --------------------------------------------------------------------
# The loader's own checks (verify.sh's custom mode) -- the symbols are the
# two libc-resident wide-conversion calls the UTF-8 path of this port
# rides (mbstowcs / wcrtomb: what turns a typed é into the codepoint
# ncursesw places as one glyph).  getopt_long and regcomp are NOT them:
# nano's gnulib layer renames everything it replaces (config.h spells
# regcomp rpl_regcomp, getopt_long rpl_getopt_long) so the libc names
# never appear in the binary.  Then the functional smoke: `nano
# --version` under prism_emu, the repo's ptrace runner, the way the
# binutils and busybox tests run their programs (compiled from
# prism_emu.c on the spot, host gcc).
do_verify() {
        [ -f "$OUT/nano" ] || do_stage
        say verify "verify.sh custom (loader contract + symbols)"
        PRISM_VERIFY_SYMBOLS="mbstowcs wcrtomb" \
        sh "$REPO/ports/toolchain/verify.sh" \
                "$WORK/nano-obj/src/nano" "$SYSROOT" custom
        say verify "nano --version under prism_emu"
        cc -O2 -Wall -o "$WORK/prism_emu" "$REPO/prism_emu.c" > /dev/null 2>&1
        # the runner's argv spelling is exec's: the first extra argument
        # becomes the guest argv[0], so the ELF path travels twice
        version=$("$WORK/prism_emu" "$OUT/nano" "$OUT/nano" --version 2>/dev/null | head -1)
        case "$version" in
        *"nano, version $NANO_VER"*)
                say verify "smoke: $version" ;;
        *)
                echo "ports/nano: prism_emu did not answer nano --version (got: $version)" >&2
                exit 1 ;;
        esac
}

case "$STAGE" in
        all)      do_fetch; do_cc; do_curses; do_nano; do_stage; do_verify ;;
        fetch)    do_fetch ;;
        cc)       do_cc ;;
        curses)   do_curses ;;
        nano)     do_nano ;;
        stage)    do_stage ;;
        verify)   do_verify ;;
        *) echo "unknown stage: $STAGE (try: list)" >&2; exit 2 ;;
esac
