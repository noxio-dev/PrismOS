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
# Nano cannot draw by itself: the curses layer is part of the port, and
# it ships DYNAMIC -- the image's first shared library beside libc.so.
# ncurses 6.6 builds libncursesw both ways in one configure (--with-shared
# --with-normal): libncursesw.a for the static twin, and the DSO
# libncursesw.so.6.6 (SONAME libncursesw.so.6, what ld.so records and
# looks up) for the image.  with_termlib defaults to no in 6.6, so the
# terminfo core, tgetent and setupterm included, lives inside it either
# way; --without-progs builds no tic / tput / toe for the target, the
# image's terminfo comes from the HOST tic compiling ports/nano/prism.src
# (the TERM=prism entry, a faithful description of console/ansi.rs).
#
# nano then builds twice against the port's own sysroot (build/nano-native/
# sysroot: the mlibc sysroot stays pristine, the way the toolchain's
# rebuild would wipe a stranger's headers):
#   - the dynamic one -- a PIE with PT_INTERP /lib/ld.so, DT_NEEDED
#     libc.so + libncursesw.so.6, BIND_NOW (the rtld cannot lazy-bind);
#     ld.so maps both DSOs at run time.  This is the binary the image
#     ships, build/nano-out/nano.
#   - a static twin -- the same link tools/prism.py makes for every app,
#     ncursesw welded in -- NOT shipped: it is the smoke vehicle, because
#     prism_emu (the repo's ptrace runner, what the binutils and busybox
#     tests drive) only implements the static-PIE contract.  It answers
#     the functional smoke, `nano --version`.
#
# The driver behind both (and the DSO) is prism-cc: three link answers --
# -shared for ncurses' MK_SHARED_LIB, PRISM_LINK_MODE=dyn for the dynamic
# PIE (the do_dyn recipe of ports/toolchain), the static sandwich by
# default.  The dynamic link is the same one do_dyn makes for hello:
# the kernel maps ld.so itself and the rtld does the rest.
#
# The ncurses side of the port is what makes accents work: mlibc's
# mbstowcs / wcrtomb decode UTF-8 unconditionally (the ports/mlibc
# decision), ncursesw counts screen columns per wide character --
# exactly what the console draws (one glyph per character, unifont) --
# and the terminfo's am-without-xenl matches the DECAWM wrap-pending
# discipline both consoles implement.
#
# The deliverable is the dynamic couple -- build/nano-out/{nano,
# libncursesw.so.6} stripped under the kernel's 4 MiB whole-file read,
# the DSO named exactly after its SONAME -- plus the terminfo; the
# Makefile's root rule copies the three into the image (the DSO beside
# ld.so and libc.so in /usr/lib, where the rtld's default paths look).
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
# the two tarballs (stage fetch).  The static twin is verified against
# the loader's contract by ports/toolchain/verify.sh (custom mode) and
# smokes under prism_emu; the dynamic couple is verified in dyn mode
# (PT_INTERP / DT_NEEDED / BIND_NOW / the PLT) extended by this port for
# the extra DSO, plus a symbol closure check: every strong undefined of
# the executable must be answered by the executable itself, libc.so or
# the ncurses DSO, and the DSO's by itself or libc.so.
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
        # The generated configure's case statements spell the shared-library
        # systems linux / gnu / k*bsd / freebsd only -- an unmatched
        # cf_cv_system_name leaves CC_SHARED_OPTS 'unknown' and the shared
        # model errors out ("Shared libraries are not supported").  Two
        # statements carry the pattern: LD_RPATH_OPT (GNU ld spells
        # -Wl,-rpath, like every ELF peer) and MK_SHARED_LIB (the
        # -shared -Wl,-soname,<soname>,-stats,-lc recipe).  Both are taught
        # the triplet the way config.sub is -- idempotent, and the longer
        # pattern is rewritten first: it contains the shorter one.
        conf="$SRC/ncurses-$NCURSES_VER/configure"
        if grep -q '(linux\*|gnu\*|k\*bsd\*-gnu|freebsd\*)' "$conf"; then
                sed -i 's/(linux\*|gnu\*|k\*bsd\*-gnu|freebsd\*)/(linux*|gnu*|k*bsd*-gnu|freebsd*|*prismos*)/' "$conf"
        fi
        if grep -q '(linux\*|gnu\*|k\*bsd\*-gnu)' "$conf"; then
                sed -i 's/(linux\*|gnu\*|k\*bsd\*-gnu)/(linux*|gnu*|k*bsd*-gnu|*prismos*)/' "$conf"
        fi
        grep -q '\*prismos\*' "$conf" \
                || { echo "ports/nano: the ncurses configure was not taught the prismos triplet" >&2; exit 1; }
        say fetch "sources ready (ncurses-$NCURSES_VER, nano-$NANO_VER)"
}

# ---- stage: cc ----------------------------------------------------------------
# prism-cc, the driver configure and make will call "the compiler" -- the
# binutils port's driver, with three link answers now:
#   -shared              a DSO link: ld -shared, no crt, no entry, no
#                        libgcc -- the shape ncurses' MK_SHARED_LIB asks
#                        for; -lc resolves to the sysroot's libc.so so
#                        the record is DT_NEEDED libc.so (the basename
#                        ld.so looks for), and -z max-page-size=4096 /
#                        separate-code keep one protection per page the
#                        way ld.so maps DSO segments
#   PRISM_LINK_MODE=dyn  a dynamic PIE: the do_dyn recipe of
#                        ports/toolchain -- PT_INTERP /lib/ld.so, -z now
#                        (BIND_NOW; the rtld zeroes GOT[1]/GOT[2] and
#                        cannot lazy-bind), the dynamic crt sandwich
#                        (Scrt1 + crti + crtbeginS ... crtn), -lc
#                        resolved against libc.so
#   (default)            the static-PIE sandwich the kernel's loader
#                        applies itself -- the twin build keeps this
# An -l resolves in the port's sysroot first (the ncurses the port
# installed), then the -L list, then the mlibc sysroot's own (libm above
# all -- mlibc ships one, and ncurses' and nano's probes spell -lm with
# no -L); the dynamic modes prefer the shared object over the archive
# (ld.so maps it at run time, the archive would weld it in).  -Wl,a,b,c
# is handed to ld comma-split, verbatim -- except -stats, an AIX-ism the
# GNU ld does not know.
do_cc() {
        mkdir -p "$WORK"
        cat > "$WORK/prism-cc" <<'EOF'
#!/bin/sh
# prism-cc -- the compiler driver of the nano port (generated by
# ports/nano/build.sh): ncurses' and nano's configure and make talk to it
# as CC, and it answers with PrismOS objects, static PIEs, dynamic PIEs
# (PRISM_LINK_MODE=dyn) and DSOs (-shared).
#   compile (-c):  x86_64-prismos-gcc, the do_prog discipline of
#                  ports/toolchain turned into driver form
#   preprocess (-E) and the dependency generators (-M...): the same
#                  discipline, no output file
#   link (-o):     -shared builds a DSO (ncurses' MK_SHARED_LIB);
#                  PRISM_LINK_MODE=dyn builds the dynamic-PIE sandwich
#                  (the do_dyn recipe of ports/toolchain); otherwise the
#                  static-PIE sandwich the kernel's loader expects --
#                  the same link tools/prism.py makes for every app
set -e
REPO=$(cd "$(dirname "$0")/../.." && pwd)
PREFIX=$REPO/build/toolchain/usr
SYSROOT=$PREFIX/x86_64-prismos
# the port's own sysroot, where the curses stage installs its headers,
# the static archives and the DSO
PORTSYS=$(cd "$(dirname "$0")/sysroot" 2>/dev/null && pwd)
CCRAW=$PREFIX/bin/x86_64-prismos-gcc
LD=$PREFIX/bin/x86_64-prismos-ld
CRTBEGIN=$($CCRAW -print-file-name=crtbeginS.o)
CRTEND=$($CCRAW -print-file-name=crtendS.o)
LIBGCC=$($CCRAW -print-libgcc-file-name)
GCCINC=$($CCRAW -print-file-name=include)
BASE="-O2 -ffreestanding -fPIE -fno-stack-protector -fno-asynchronous-unwind-tables -fno-math-errno"
LIBCSO=$SYSROOT/usr/lib/libc.so

compile() {
        exec "$CCRAW" $BASE -nostdinc -isystem "$GCCINC" \
                -isystem "$SYSROOT/usr/include" "$@"
}

# ---- the link -----------------------------------------------------------------
# configure's probes hand the driver a .c (compile and link in one
# command); make's link rules hand it objects and archives.  Both end in
# the link the mode asks for.
link() {
        out= objs= archives= libs= ldirs= srcs= tmpobjs= wflags= ldflags=
        mode=${PRISM_LINK_MODE:-static}
        case $mode in
        static|dyn) ;;
        *) echo "prism-cc: bad PRISM_LINK_MODE '$mode'" >&2; exit 1 ;;
        esac
        while [ $# -gt 0 ]; do
                case "$1" in
                -o)         out=$2; shift 2 ;;
                --version|-v|-V|-qversion|-dumpversion|-dumpmachine|-print-*)
                            exec "$CCRAW" "$@" ;;
                -shared)    mode=dso; shift ;;
                -Wl,*)      oifs=$IFS; IFS=,
                            for w in ${1#-Wl,}; do
                                # -stats is an AIX-ism (trace the dynamic
                                # linker's activity); GNU ld does not know it
                                [ "$w" = -stats ] && continue
                                wflags="$wflags $w"
                            done
                            IFS=$oifs; shift ;;
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

        # the -Wl parts, verbatim to ld: -soname takes the word after it
        # as its value (the caller's value, ncurses' ABI versioning), a
        # -l / -L joins the same resolution the bare spellings go
        # through, anything else rides in order (-rpath-link's argument
        # follows it like ld expects)
        soname=
        prev=
        prevkind=
        for w in $wflags; do
                if [ -n "$prev" ]; then
                        case $prevkind in
                        soname) soname=$w ;;
                        l)      libs="$libs $w" ;;
                        esac
                        prev=
                        prevkind=
                        continue
                fi
                case $w in
                -soname) prev=$w; prevkind=soname ;;
                -l)      prev=$w; prevkind=l ;;
                -l*)     libs="$libs ${w#-l}" ;;
                -L*)     ldirs="$ldirs ${w#-L}" ;;
                *)       ldflags="$ldflags $w" ;;
                esac
        done
        if [ -n "$prev" ]; then
                echo "prism-cc: -Wl,$prev without a value" >&2
                exit 1
        fi

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

        # -lfoo resolves against the port's sysroot first (the ncurses the
        # port installed), then the -L list, then the mlibc sysroot's own
        # (libm -- mlibc ships one, and the probes spell -lm with no -L
        # to carry it).  The dynamic modes prefer the shared object (the
        # DSO ld.so maps at run time; the archive would weld it in),
        # falling back to the archive only for the library files mlibc
        # installs PIC (meson's default_library=both); the DSO link has
        # no archive fallback at all -- a DSO must not weld archive code.
        resolve() { # name -> path (echoed) or nothing
                l=$1
                if [ "$mode" != static ]; then
                        for d in "$PORTSYS" $ldirs "$SYSROOT/usr/lib"; do
                                if [ -n "$d" ] && [ -f "$d/lib$l.so" ]; then
                                        echo "$d/lib$l.so"
                                        return
                                fi
                                for s in "$d/lib$l".so.*; do
                                        if [ -f "$s" ]; then
                                                echo "$s"
                                                return
                                        fi
                                done
                        done
                        if [ "$mode" = dso ]; then
                                return
                        fi
                fi
                for d in $ldirs "$SYSROOT/usr/lib"; do
                        if [ -f "$d/lib$l.a" ]; then
                                echo "$d/lib$l.a"
                                return
                        fi
                done
        }
        resolved=
        for l in $libs; do
                found=$(resolve "$l")
                if [ -z "$found" ]; then
                        rm -f $tmpobjs
                        echo "prism-cc: cannot resolve -l$l" >&2
                        exit 1
                fi
                resolved="$resolved $found"
        done

        if [ "$mode" = dso ]; then
                # ---- the DSO: no crt, no entry, no libgcc -- ld -shared
                # only.  -z norelro: the rtld ignores PT_GNU_RELRO of a
                # DSO; -z max-page-size=4096 / separate-code: one
                # protection per page, the way ld.so maps segments with
                # file mmap.  The soname defaults to the output's
                # basename (the file ld.so looks for).
                if [ -z "$soname" ]; then soname=${out##*/}; fi
                if "$LD" -m elf_x86_64 -shared -z max-page-size=4096 \
                        -z separate-code -z noexecstack -z norelro \
                        -soname "$soname" -o "$out" \
                        $objs $archives $resolved $ldflags; then
                        rc=0
                else
                        rc=$?
                fi
                rm -f $tmpobjs
                exit $rc
        fi

        if [ "$mode" = dyn ]; then
                # ---- the dynamic PIE: the do_dyn recipe of
                # ports/toolchain.  PT_INTERP /lib/ld.so (the kernel maps
                # the rtld at USER_INTERP_BASE and hands it AT_BASE), -z
                # now -- BIND_NOW, because the rtld zeroes GOT[1]/GOT[2]
                # and cannot lazy-bind -- and the dynamic crt sandwich.
                # libc.so leads the group once (deduped against the -lc
                # resolution) and the port's own archives ride with it
                # (gnulib's libgnu.a: nano's rpl_* renames live there);
                # the rpath-links carry ld over the two sysroots while
                # it traces DT_NEEDED chains.
                case " $resolved " in
                *" $LIBCSO "*) ;;
                *) resolved="$resolved $LIBCSO" ;;
                esac
                rplinks=
                if [ -n "$PORTSYS" ]; then
                        rplinks="-rpath-link $PORTSYS/usr/lib"
                fi
                if "$LD" -m elf_x86_64 -pie \
                        --dynamic-linker /lib/ld.so -z now -z separate-code \
                        -z max-page-size=4096 -z noexecstack -z norelro -e _start \
                        -o "$out" \
                        "$SYSROOT/usr/lib/Scrt1.o" "$SYSROOT/usr/lib/crti.o" \
                        "$CRTBEGIN" \
                        $objs \
                        --start-group $resolved $archives "$LIBGCC" --end-group \
                        "$CRTEND" "$SYSROOT/usr/lib/crtn.o" \
                        $rplinks -rpath-link "$SYSROOT/usr/lib" $ldflags; then
                        rc=0
                else
                        rc=$?
                fi
                rm -f $tmpobjs
                exit $rc
        fi

        # ---- the static sandwich of tools/prism.py's compile_and_link:
        # the twin build, the smoke vehicle prism_emu can run.  libc.a
        # leads the group: the programs ask the C library for getopt_long
        # / regcomp / the math before anything else is scanned, and the
        # group keeps the archives' mutual dependencies satisfiable in
        # both directions.
        if "$LD" -m elf_x86_64 -pie --no-dynamic-linker -z separate-code \
                -z max-page-size=4096 -z noexecstack -z norelro -e _start \
                -o "$out" \
                "$SYSROOT/usr/lib/Scrt1.o" "$SYSROOT/usr/lib/crti.o" \
                "$CRTBEGIN" \
                $objs \
                --start-group "$SYSROOT/usr/lib/libc.a" \
                $archives $resolved "$LIBGCC" --end-group \
                "$CRTEND" "$SYSROOT/usr/lib/crtn.o" $ldflags; then
                rc=0
        else
                rc=$?
        fi
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
        say cc "$WORK/prism-cc (compile: $PREFIX/bin/x86_64-prismos-gcc, links: DSO / dynamic PIE / static-PIE sandwich)"
}

# ---- stage: curses --------------------------------------------------------------
# ncurses 6.6 for the target, BOTH library models in one configure:
# --with-shared --with-normal builds libncursesw.a (the wide/UTF-8 API,
# terminfo core included) AND the DSO libncursesw.so.6.6 -- SONAME
# libncursesw.so.6, what ld.so records from DT_NEEDED and looks up in
# /usr/lib at run time.  The shared model's objects (../obj_s) would get
# their -fPIC from CC_SHARED_OPTS, but that probe only answers for
# gcc-like compilers (GCC=no here: the driver is clang behind a wrapper)
# and *linux* system names (cf_cv_system_name is x86_64-prismos) -- so
# -fPIC rides in CFLAGS for every object, static model included (PIC
# archives link into the static twin just as well).  --without-progs (no
# target tic / tput / toe: the image gets its terminfo from the host tic
# below) and --disable-db-install (the entry travels with the port).
# The libraries, the DSO and headers install into the port's own sysroot
# -- build/nano-native/sysroot -- so the mlibc sysroot stays pristine.
do_curses() {
        [ -d "$SRC/ncurses-$NCURSES_VER" ] || do_fetch
        [ -x "$WORK/prism-cc" ] || do_cc
        rm -rf "$WORK/ncurses-obj"
        mkdir -p "$WORK/ncurses-obj"
        cd "$WORK/ncurses-obj"
        say curses "configure --host=x86_64-prismos (widec, shared + normal, no progs, no db)"
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
            CFLAGS="-O2 -D_GNU_SOURCE -fPIC" \
            LDFLAGS="" \
        "$SRC/ncurses-$NCURSES_VER/configure" \
                --build="$BUILD" \
                --host=x86_64-prismos \
                --prefix=/usr \
                --enable-widec \
                --without-cxx --without-cxx-binding \
                --without-progs --without-tack --without-tests \
                --without-manpages \
                --with-shared --with-normal \
                --without-gpm --without-dlsym \
                --disable-db-install \
                --with-terminfo-dirs=/usr/share/terminfo \
                --with-default-terminfo-dir=/usr/share/terminfo \
                --with-build-cc="$BUILD_CC" --with-build-cflags="-O2" \
                > "$WORK/ncurses-configure.log" 2>&1 \
                || { tail -30 "$WORK/ncurses-configure.log"; exit 1; }
        say curses "make -j$JOBS (libncursesw.a + libncursesw.so.6.6)"
        make -j"$JOBS" > "$WORK/ncurses-build.log" 2>&1 \
                || { tail -40 "$WORK/ncurses-build.log"; exit 1; }
        # the DSO must have been linked through the driver's -shared mode:
        # the build log carries the MK_SHARED_LIB line, the lib directory
        # the DSO itself
        grep -q -- '-shared' "$WORK/ncurses-build.log" \
                || { echo "ports/nano: no DSO link in the ncurses build" >&2; exit 1; }
        dsofile=$(ls "$WORK/ncurses-obj"/lib/libncursesw.so.* 2>/dev/null | head -1)
        [ -f "$dsofile" ] || { echo "ports/nano: libncursesw.so.* missing" >&2; exit 1; }
        for lib in libncursesw.a; do
                [ -f "lib/$lib" ] || { echo "ports/nano: lib/$lib missing" >&2; exit 1; }
        done
        say curses "install into the port's sysroot"
        make -j1 install.libs DESTDIR="$WORK/sysroot" \
                > "$WORK/ncurses-install.log" 2>&1 \
                || { tail -20 "$WORK/ncurses-install.log"; exit 1; }
        [ -f "$WORK/sysroot/usr/lib/libncursesw.a" ] \
                || { echo "ports/nano: libncursesw.a not installed" >&2; exit 1; }
        dsoinst=$(ls "$WORK"/sysroot/usr/lib/libncursesw.so.* 2>/dev/null | head -1)
        [ -f "$dsoinst" ] || { echo "ports/nano: the DSO not installed" >&2; exit 1; }
        soname=$($PREFIX/bin/x86_64-prismos-readelf -dW "$dsoinst" \
                | sed -n 's/.*SONAME.*\[\(.*\)\]/\1/p')
        [ "$soname" = "libncursesw.so.$(echo "$NCURSES_VER" | cut -d. -f1)" ] \
                || { echo "ports/nano: DSO SONAME is '$soname', wanted libncursesw.so.$(echo "$NCURSES_VER" | cut -d. -f1)" >&2; exit 1; }
        say curses "$WORK/sysroot/usr/lib: libncursesw.a (terminfo core inside) + $dsoinst (SONAME $soname), headers in usr/include"
}

# ---- stage: nano ------------------------------------------------------------------
# nano 9.3 against the port's ncurses: the header and library paths ride
# CFLAGS / LDFLAGS (the driver passes -I through on compiles, resolves
# the -l list at link), PKG_CONFIG is a deliberate false so configure's
# ncursesw probe cannot ask the HOST's ncurses, and -lncursesw carries
# everything -- curses and the terminfo core alike (see stage curses).
# Two builds:
#   nano-obj-static -- the static twin (PRISM_LINK_MODE=static): the
#      sandwich the kernel's loader applies itself, ncursesw welded in;
#      the smoke vehicle prism_emu can run, never shipped
#   nano-obj -- the image's editor (PRISM_LINK_MODE=dyn): a PIE with
#      PT_INTERP /lib/ld.so, DT_NEEDED libc.so + libncursesw.so.6,
#      BIND_NOW -- the do_dyn link of ports/toolchain
# The dyn build's checks: the DSO the driver recorded, and libc.so.
do_nano() {
        [ -f "$WORK/sysroot/usr/lib/libncursesw.a" ] || do_curses
        [ -d "$SRC/nano-$NANO_VER" ] || do_fetch
        [ -x "$WORK/prism-cc" ] || do_cc
        for build in nano-obj-static nano-obj; do
                case $build in
                nano-obj-static) mode=static; say nano "static twin (the prism_emu smoke vehicle, not shipped)" ;;
                nano-obj)        mode=dyn;    say nano "the image's editor (dynamic PIE: PT_INTERP /lib/ld.so, DT_NEEDED libc.so + libncursesw.so.6)" ;;
                esac
                rm -rf "$WORK/$build"
                mkdir -p "$WORK/$build"
                cd "$WORK/$build"
                say nano "configure --host=x86_64-prismos (the port's ncursesw, no NLS, no speller)"
                BUILD=$(sh "$SRC/nano-$NANO_VER/config.guess")
                env PRISM_LINK_MODE="$mode" \
                    CC="$WORK/prism-cc" \
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
                        > "$WORK/$build-configure.log" 2>&1 \
                        || { tail -30 "$WORK/$build-configure.log"; exit 1; }
                say nano "make -j$JOBS (src/nano, PRISM_LINK_MODE=$mode)"
                PRISM_LINK_MODE="$mode" \
                make -j"$JOBS" > "$WORK/$build-build.log" 2>&1 \
                        || { tail -40 "$WORK/$build-build.log"; exit 1; }
                [ -f "src/nano" ] || { echo "ports/nano: $build/src/nano missing" >&2; exit 1; }
        done
        needed=$($PREFIX/bin/x86_64-prismos-readelf -dW "$WORK/nano-obj/src/nano" | grep '(NEEDED)' | sed 's/.*\[\(.*\)\]/\1/' | tr '\n' ' ')
        case "$needed" in
        *libc.so*libncursesw.so.6*|*libncursesw.so.6*libc.so*)
                say nano "dyn: DT_NEEDED $(echo $needed | tr -s ' ')" ;;
        *)
                echo "ports/nano: the dynamic nano needs [$(echo $needed)] -- libc.so + libncursesw.so.6 wanted" >&2
                exit 1 ;;
        esac
        say nano "built: nano-obj/src/nano (dynamic) + nano-obj-static/src/nano (twin)"
}

# ---- stage: stage -------------------------------------------------------------------
# The deliverable is the dynamic couple -- build/nano-out/{nano,
# libncursesw.so.6}, both stripped under the kernel's 4 MiB whole-file
# read.  The DSO's FILE name must be exactly its SONAME: DT_NEEDED says
# libncursesw.so.6 and that is the basename ld.so looks for in /usr/lib,
# so the strip lands on the readelf-recorded SONAME.  The static twin
# (nano-static) stages beside them for the smoke only; the terminfo
# entry, the host tic compiling ports/nano/prism.src.
do_stage() {
        [ -f "$WORK/nano-obj/src/nano" ] || do_nano
        mkdir -p "$OUT"
        say stage "stripping nano (dynamic) + the static twin + the DSO"
        $PREFIX/bin/x86_64-prismos-strip --strip-all -o "$OUT/nano" "$WORK/nano-obj/src/nano"
        $PREFIX/bin/x86_64-prismos-strip --strip-all -o "$OUT/nano-static" "$WORK/nano-obj-static/src/nano"
        dsoinst=$(ls "$WORK"/sysroot/usr/lib/libncursesw.so.* 2>/dev/null | head -1)
        [ -f "$dsoinst" ] || { echo "ports/nano: the DSO not installed" >&2; exit 1; }
        soname=$($PREFIX/bin/x86_64-prismos-readelf -dW "$dsoinst" \
                | sed -n 's/.*SONAME.*\[\(.*\)\]/\1/p')
        $PREFIX/bin/x86_64-prismos-strip --strip-all -o "$OUT/$soname" "$dsoinst"
        [ -f "$OUT/$soname" ] || { echo "ports/nano: the DSO strip failed" >&2; exit 1; }
        say stage "the terminfo (host tic -> $TERMINFO_FILE)"
        rm -rf "$OUT/terminfo"
        tic -o "$OUT/terminfo" "$REPO/ports/nano/prism.src" > "$WORK/tic.log" 2>&1 \
                || { cat "$WORK/tic.log"; exit 1; }
        [ -f "$OUT/terminfo/$TERMINFO_FILE" ] \
                || { echo "ports/nano: terminfo/$TERMINFO_FILE missing" >&2; exit 1; }
        say stage "$OUT: $(ls "$OUT" | tr '\n' ' ')"
}

# ---- stage: verify --------------------------------------------------------------------
# Three layers:
#   1. the static twin through verify.sh's custom mode -- the loader's
#      own checks plus the two libc-resident wide-conversion calls the
#      UTF-8 path of this port rides (mbstowcs / wcrtomb: what turns a
#      typed é into the codepoint ncursesw places as one glyph).
#      getopt_long and regcomp are NOT them: nano's gnulib layer renames
#      everything it replaces (config.h spells regcomp rpl_regcomp,
#      getopt_long rpl_getopt_long) so the libc names never appear in
#      the binary.  Then the functional smoke: `nano --version` under
#      prism_emu, the repo's ptrace runner, the way the binutils and
#      busybox tests run their programs (compiled from prism_emu.c on
#      the spot, host gcc).  The runner only implements the static-PIE
#      contract, so it can NEVER run the dynamic binary -- that is what
#      the twin is staged for.
#   2. the dynamic couple through verify.sh's dyn mode, extended by this
#      port (PRISM_VERIFY_*): two DT_NEEDED records instead of one, no
#      prism_raw trio (nano does not ride prism_raw.h), and the ncurses
#      DSO checked the way ld.so is: ET_DYN, no PT_INTERP, its SONAME,
#      its exports.
#   3. the symbol closure, checked statically: every strong undefined of
#      the executable must be answered by the executable itself, libc.so
#      or the ncurses DSO, and the DSO's by itself or libc.so -- the
#      promise the rtld would otherwise only keep at run time.
do_verify() {
        [ -f "$OUT/nano" ] || do_stage
        RE=$PREFIX/bin/x86_64-prismos-readelf
        say verify "the static twin: verify.sh custom (loader contract + symbols)"
        PRISM_VERIFY_SYMBOLS="mbstowcs wcrtomb" \
        sh "$REPO/ports/toolchain/verify.sh" \
                "$WORK/nano-obj-static/src/nano" "$SYSROOT" custom
        say verify "smoke: nano --version under prism_emu (the twin -- the runner is static-PIE only)"
        cc -O2 -Wall -o "$WORK/prism_emu" "$REPO/prism_emu.c" > /dev/null 2>&1
        # the runner's argv spelling is exec's: the first extra argument
        # becomes the guest argv[0], so the ELF path travels twice
        version=$("$WORK/prism_emu" "$OUT/nano-static" "$OUT/nano-static" --version 2>/dev/null | head -1)
        case "$version" in
        *"nano, version $NANO_VER"*)
                say verify "smoke: $version" ;;
        *)
                echo "ports/nano: prism_emu did not answer nano --version (got: $version)" >&2
                exit 1 ;;
        esac
        dsofile=$(ls "$OUT"/libncursesw.so.* | head -1)
        soname=$($RE -dW "$dsofile" | sed -n 's/.*SONAME.*\[\(.*\)\]/\1/p')
        say verify "the dynamic couple: verify.sh dyn (PT_INTERP, DT_NEEDED, BIND_NOW, the PLT + the DSO)"
        PRISM_VERIFY_NEEDED="libc.so $soname" \
        PRISM_VERIFY_RAW=0 \
        PRISM_VERIFY_DSO="$dsofile" \
        PRISM_VERIFY_DSO_SONAME="$soname" \
        PRISM_VERIFY_DSO_SYMBOLS="initscr endwin tgetent setupterm" \
        sh "$REPO/ports/toolchain/verify.sh" "$OUT/nano" "$SYSROOT" dyn
        say verify "symbol closure (the rtld's own promise, checked statically)"
        und_of() { $RE --dyn-syms -W "$1" | awk '$7 == "UND" && $5 != "WEAK" && $8 != "" {print $8}' | sort -u; }
        def_of() { $RE --dyn-syms -W "$1" | awk '$7 != "UND" && $8 != "" {print $8}' | sort -u; }
        {
                def_of "$OUT/nano"
                def_of "$SYSROOT/usr/lib/libc.so"
                def_of "$dsofile"
        } | sort -u > "$WORK/exports.txt"
        miss=
        for s in $(und_of "$OUT/nano"); do
                grep -qx "$s" "$WORK/exports.txt" || miss="$miss $s"
        done
        if [ -n "$miss" ]; then
                echo "ports/nano: unresolved dynamic symbols in the executable:$miss" >&2
                exit 1
        fi
        say verify "exec: every strong UND answered (by itself, libc.so or the ncurses DSO)"
        {
                def_of "$SYSROOT/usr/lib/libc.so"
                def_of "$dsofile"
        } | sort -u > "$WORK/dso-exports.txt"
        miss=
        for s in $(und_of "$dsofile"); do
                grep -qx "$s" "$WORK/dso-exports.txt" || miss="$miss $s"
        done
        if [ -n "$miss" ]; then
                echo "ports/nano: unresolved dynamic symbols in the DSO:$miss" >&2
                exit 1
        fi
        say verify "DSO: every strong UND answered (by libc.so or the DSO itself)"
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
