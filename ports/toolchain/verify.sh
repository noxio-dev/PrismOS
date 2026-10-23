#!/bin/sh
# readelf/objdump print translated text under a non-English locale; the
# checks below parse the English wording.
LC_ALL=C; export LC_ALL
#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# The step-3 verification (phase 3.2): everything that can be checked about
# an mlibc-built program without booting the machine.  The checks are the
# ones the kernel's ELF loader (sys/kern/src/user/elf.rs) actually makes:
#
#   - ET_DYN (static PIE) with a page-aligned segment set
#   - no PT_INTERP (in the *static* modes -- dynamic linking is the phase's
#     last step, and it has a mode of its own, below)
#   - a PT_DYNAMIC carrying DT_RELA / DT_RELASZ (the R_X86_64_RELATIVE
#     relocations the loader applies at USER_BASE), no DT_NEEDED, no
#     DT_JMPREL (the loader refuses those three -- in static mode)
#   - under disassembly, the port's own ABI: every syscall the program
#     makes reaching its number -- the tag layer passes it as
#     __do_syscallN's first argument (`mov $nr,%edi ... call
#     <__do_syscallN>`), hand-written asm (the restorer, the raw shim of
#     usr/mlibc/prism_raw.h) goes direct (`mov $nr,%eax ... syscall`) --
#     plus the symbols those paths live in
#
# With a program name (hello | fork | fd | posix -- the phase 3.2, step 4
# ports of usr/mlibc) the syscall and symbol set is that program's; without
# one, the step-3 proof set applies (write / the restorer / clock_gettime).
#
# The `dyn` mode (phase 3.2's last step) verifies the whole dynamic-linking
# chain instead, still statically:
#   - the executable: PT_INTERP /lib/ld.so, DT_NEEDED libc.so, DT_JMPREL
#     (the PLT the rtld resolves), BIND_NOW (mlibc's rtld zeroes
#     GOT[1]/GOT[2]: lazy binding is not an option), JUMP_SLOT relocs,
#     __mlibc_entry left undefined for libc.so to answer
#   - ld.so (the sysroot's /usr/lib/ld.so): a self-contained ET_DYN -- no
#     PT_INTERP of its own, SONAME ld.so, R_X86_64_RELATIVE relocations
#     *only* (relocateSelf traps on anything with a symbol), no undefined
#     symbols, and under disassembly its exact syscall diet: open / read /
#     close / lseek / fstat (the library files), mmap / munmap / mprotect
#     (the DSO reservations, the RELRO and segment protections),
#     arch_prctl (TcbSet: the FS base the TLS lives on), getpid (the tid)
#   - libc.so: a shared ET_DYN exporting __mlibc_entry / printf /
#     __do_syscall6 / malloc, carrying the hello program's syscalls (now
#     that they live in the library, not the binary) and the restorer
#
# Usage: verify.sh <elf> [<sysroot>] [<program> | dyn]
set -e

BIN=${1:?usage: verify.sh <elf> [<sysroot>] [<program> | dyn]}
PROG=${3:-}
READELF=${READELF:-readelf}
OBJDUMP=${OBJDUMP:-objdump}
NM=${NM:-nm}

fail=0
ok()   { printf '  ok   %s\n' "$1"; }
bad()  { printf '  FAIL %s\n' "$1"; fail=$((fail + 1)); }

# The syscall each number is, for the report.
scname() {
        case $1 in
        1) echo write;; 3) echo uptime_ms;; 4) echo yield;; 5) echo sleep;;
        7) echo execve;; 8) echo fork;; 9) echo wait;; 10) echo kill;;
        12) echo sigreturn;; 13) echo read;; 14) echo open;; 15) echo close;;
        16) echo lseek;; 17) echo dup;; 18) echo pipe;; 19) echo stat;;
        20) echo fstat;; 21) echo brk;; 22) echo mmap;; 23) echo munmap;;
        24) echo chdir;; 25) echo getcwd;; 26) echo dup2;; 27) echo fcntl;;
        28) echo getdents;; 29) echo ioctl;; 30) echo unlink;; 31) echo mkdir;;
        32) echo rmdir;; 33) echo rename;; 34) echo link;; 35) echo uname;;
        36) echo arch_prctl;; 38) echo sigaction;; 40) echo mprotect;;
        43) echo clock_gettime;;
        *) echo "syscall $1";; esac
}

# The port's calling convention: the tag layer passes the syscall number as
# the first argument (__do_syscallN's %edi: `mov $nr,%edi ... call
# <__do_syscallN>`), while hand-written asm (the restorer, the raw shim of
# usr/mlibc/prism_raw.h) goes direct (`mov $nr,%eax ... syscall`).  Track
# both, hex-compared, one register-copy hop deep: the optimizer hoists a
# loop's syscall number into a callee-saved register (`mov $4,%r12d` before
# the loop, `mov %r12,%rax; syscall` inside it), so an immediate loaded
# into any register travels with register-to-register moves.  Anything else
# that writes a tracked register -- an arithmetic writer, a memory load, a
# call's caller-saved clobber -- retires the tracked immediate: it is stale
# the moment the register is rewritten.
reaches_file() { # nr file
        hex=$(printf '%x' "$1")
        $OBJDUMP -d "$2" | awk -v nr="$hex" '
                function reg(r) {
                        if (r ~ /^r[0-9]+d$/) sub(/d$/, "", r)
                        else if (r ~ /^e(ax|bx|cx|dx|si|di|bp|sp)$/) r = "r" substr(r, 2)
                        return r
                }
                function retire(r) { delete known[r] }
                function hexval(h,   i, c, v) {
                        v = 0; h = tolower(h)
                        for (i = 1; i <= length(h); i++) {
                                c = index("0123456789abcdef", substr(h, i, 1)) - 1
                                v = v * 16 + c
                        }
                        return v
                }
                /^[0-9a-f]+ <.*>:$/ { delete known; delete fnrdi; sawset = 0 }
                {
                        line = $0
                        sub(/#.*/, "", line)   # strip the symbol comments
                        if (line ~ /mov[[:space:]]+\$0x[0-9a-f]+,%[a-z0-9]+/) {
                                match(line, /\$0x[0-9a-f]+/)
                                imm = substr(line, RSTART + 3, RLENGTH - 3)
                                match(line, /,%[a-z0-9]+$/)
                                d = reg(substr(line, RSTART + 2, RLENGTH - 2))
                                known[d] = imm
                                if (d == "rdi") fnrdi[imm] = 1
                                next
                        }
                        if (line ~ /mov[[:space:]]+%[a-z0-9]+,%[a-z0-9]+$/) {
                                match(line, /mov[[:space:]]+%[a-z0-9]+,/)
                                src = reg(substr(line, RSTART, RLENGTH))
                                gsub(/^mov[[:space:]]+|%|,$/, "", src)
                                match(line, /,%[a-z0-9]+$/)
                                dst = reg(substr(line, RSTART + 2, RLENGTH - 2))
                                if (src in known) known[dst] = known[src]
                                else retire(dst)
                                next
                        }
                        # a boolean selecting between two constants: setcc %al then
                        # lea K(,%rax,S),%rdi is K or K+S (clang folds `c ? A : B` this way)
                        if (line ~ /[[:space:]]set[a-z]+[[:space:]]/) sawset = 1
                        if (sawset && line ~ /lea[[:space:]]+0x[0-9a-f]+\(,%r[a-z0-9]+,[1248]\),%(rdi|edi)$/) {
                                match(line, /0x[0-9a-f]+\(/)
                                disp = hexval(substr(line, RSTART + 2, RLENGTH - 3))
                                match(line, /,[1248]\)/)
                                scale = substr(line, RSTART + 1, 1) + 0
                                fnrdi[sprintf("%x", disp)] = 1
                                fnrdi[sprintf("%x", disp + scale)] = 1
                                retire("rdi")
                                next
                        }
                        # any other instruction writing a tracked register
                        # (lea, arithmetic, memory loads, a self-xor) retires
                        # it -- except the pure readers (cmp / test), whose
                        # last operand is read, not written
                        if (line !~ /(^|[[:space:]])(cmp|test)[[:space:]]/ \
                            && match(line, /,%(r(ax|bx|bp|cx|dx|si|di|sp)|r1[0-5]|r[89])(d|w|b)?$/))
                                retire(reg(substr(line, RSTART + 2, RLENGTH - 2)))
                        if (line ~ /call[[:space:]].*<__do_syscall/) {
                                if ((("rdi" in known) && known["rdi"] == nr) || (nr in fnrdi)) found = 1
                                # the call clobbers every caller-saved register
                                retire("rax"); retire("rcx"); retire("rdx")
                                retire("rsi"); retire("rdi")
                                for (i = 8; i <= 11; i++) retire("r" i)
                                next
                        }
                        if (line ~ /call[[:space:]]/) {
                                retire("rax"); retire("rcx"); retire("rdx")
                                retire("rsi"); retire("rdi")
                                for (i = 8; i <= 11; i++) retire("r" i)
                                next
                        }
                        if (line ~ /^[[:space:]]*[0-9a-f]+:[[:space:]].*syscall/) {
                                if (("rax" in known) && known["rax"] == nr) found = 1
                        }
                }
                END { exit !found }'
}
reaches() { reaches_file "$1" "$BIN"; }

# --------------------------------------------------------------------- dyn
# The dynamic-linking verification (phase 3.2's last step).  Reachable as
# `verify.sh <dynamic-elf> <sysroot> dyn` (ports/toolchain/build.sh's
# dynverify stage); <sysroot> is where ld.so and libc.so are looked up.
if [ "$PROG" = dyn ]; then
        LDSO=${2:?verify.sh dyn: <sysroot> required}/usr/lib/ld.so
        LIBC=${2:?}/usr/lib/libc.so

        echo "== the executable (what the loader's PT_INTERP path checks) =="

        type=$($READELF -h "$BIN" | sed -n 's/.*Type:.*\(DYN\|EXEC\).*/\1/p')
        [ "$type" = DYN ] && ok "ET_DYN (dynamic PIE)" || bad "ELF type is $type, wanted DYN"

        interp=$($READELF -lW "$BIN" | sed -n 's/.*Requesting program interpreter: \(.*\)\]/\1/p' | head -1)
        [ "$interp" = /lib/ld.so ] \
                && ok "PT_INTERP /lib/ld.so (the kernel loads it at USER_INTERP_BASE)" \
                || bad "PT_INTERP is '${interp:-missing}'"

        dyn=$($READELF -lW "$BIN" | grep -c ' DYNAMIC ' || true)
        [ "$dyn" -ge 1 ] && ok "has PT_DYNAMIC" || bad "no PT_DYNAMIC"

        needed=$($READELF -dW "$BIN" | grep '(NEEDED)' | grep -c 'libc\.so' || true)
        [ "$needed" -ge 1 ] \
                && ok "DT_NEEDED libc.so (ld.so finds it in /lib and /usr/lib)" \
                || bad "no DT_NEEDED libc.so"

        jmprel=$($READELF -dW "$BIN" | grep -c '(JMPREL)' || true)
        [ "$jmprel" -ge 1 ] \
                && ok "has DT_JMPREL (the PLT -- the rtld's to resolve now)" \
                || bad "no DT_JMPREL"

        now=$($READELF -dW "$BIN" | grep 'FLAGS_1' | grep -cw 'NOW' || true)
        [ "$now" -ge 1 ] \
                && ok "BIND_NOW (the rtld zeroes GOT[1]/GOT[2]: no lazy binding)" \
                || bad "no BIND_NOW"

        irel=$($READELF -rW "$BIN" | grep -c 'IRELATIVE' || true)
        [ "$irel" = 0 ] && ok "no IRELATIVE (no IFUNCs: the port builds them off)" \
                || bad "has IRELATIVE relocations (an IFUNC slipped in)"

        js=$($OBJDUMP -R "$BIN" | grep -c 'R_X86_64_JUMP_SLOT' || true)
        [ "$js" -gt 0 ] && ok "$js JUMP_SLOT relocations (resolved at startup)" \
                || bad "no JUMP_SLOT relocations"

        $NM -D "$BIN" 2>/dev/null | grep -q 'U __mlibc_entry$' \
                && ok "__mlibc_entry left undefined (libc.so answers it)" \
                || bad "__mlibc_entry is not an undefined dynamic reference"

        # The loader's geometry rule (elf.rs), the same check as the static
        # modes: no two segments share a page.
        shared=0
        prev_v= prev_m=
        while read -r v m; do
                v=$((0x${v#0x})); m=$((0x${m#0x}))
                if [ -n "$prev_v" ]; then
                        e=$((prev_v + prev_m + 4095))
                        prev_end=$((e - e % 4096))
                        down=$((v - v % 4096))
                        [ "$down" -lt "$prev_end" ] && shared=$((shared + 1))
                fi
                prev_v=$v
                prev_m=$m
        done <<EOF
$($READELF -lW "$BIN" | awk '/^ *LOAD/ {print $3, $6}')
EOF
        [ "$shared" = 0 ] \
                && ok "no two segments share a page (one protection per page, W^X)" \
                || bad "$shared segment pair(s) share a page"

        exec_stack=$($READELF -lW "$BIN" | grep -c 'GNU_STACK.*RWE' || true)
        [ "$exec_stack" = 0 ] && ok "non-executable stack" || bad "executable stack"

        echo "== the executable under disassembly (the raw shim stays raw) =="

        # The PrismOS-only calls of usr/mlibc/prism_raw.h are inlined into the
        # program itself -- dynamic linking changes where the *library* calls
        # live, not these (uptime_ms / yield / the millisecond sleep: 3 4 5).
        for n in 3 4 5; do
                if reaches_file "$n" "$BIN"; then
                        ok "$(scname "$n") reaches syscall $n (prism_raw, in the binary)"
                else
                        bad "no syscall $n ($(scname "$n")) in the binary"
                fi
        done

        echo "== ld.so (the interpreter the kernel maps) =="

        if [ -f "$LDSO" ]; then
                ok "$LDSO installed"
        else
                bad "$LDSO missing"
        fi
        type=$($READELF -h "$LDSO" | sed -n 's/.*Type:.*\(DYN\|EXEC\).*/\1/p')
        [ "$type" = DYN ] && ok "ld.so is ET_DYN (mapped at USER_INTERP_BASE)" \
                || bad "ld.so ELF type is $type"
        interp=$($READELF -lW "$LDSO" | grep -c INTERP || true)
        [ "$interp" = 0 ] && ok "ld.so has no PT_INTERP (self-contained)" \
                || bad "ld.so has a PT_INTERP"
        soname=$($READELF -dW "$LDSO" | grep '(SONAME)' | sed 's/.*\[\(.*\)\]/\1/')
        [ "$soname" = ld.so ] && ok "SONAME ld.so" || bad "SONAME is '$soname'"
        nrel=$($OBJDUMP -R "$LDSO" | grep -c 'R_X86_64_RELATIVE' || true)
        other=$($OBJDUMP -R "$LDSO" | grep 'R_X86_' | grep -vc 'R_X86_64_RELATIVE' || true)
        [ "$nrel" -gt 0 ] && [ "$other" = 0 ] \
                && ok "$nrel R_X86_64_RELATIVE relocations, nothing else (relocateSelf's contract)" \
                || bad "ld.so relocations are not RELATIVE-only ($nrel RELATIVE, $other other)"
        undef=$($READELF --dyn-syms -W "$LDSO" | awk '$7 == "UND" && $8 != ""' | wc -l)
        [ "$undef" = 0 ] && ok "no undefined symbols (-nostdlib against nothing)" \
                || bad "$undef undefined symbol(s)"

        echo "== ld.so under disassembly (its syscall diet) =="

        # Every syscall the rtld itself makes, through the same tag-layer
        # convention the static modes track.
        for n in 2 13 14 15 16 20 22 23 36 40; do
                if reaches_file "$n" "$LDSO"; then
                        ok "$(scname "$n") reaches syscall $n"
                else
                        bad "no syscall $n ($(scname "$n")) in ld.so"
                fi
        done

        echo "== libc.so (the library the rtld maps) =="

        if [ -f "$LIBC" ]; then
                ok "$LIBC installed"
        else
                bad "$LIBC missing"
        fi
        type=$($READELF -h "$LIBC" | sed -n 's/.*Type:.*\(DYN\|EXEC\).*/\1/p')
        [ "$type" = DYN ] && ok "libc.so is ET_DYN" || bad "libc.so ELF type is $type"
        interp=$($READELF -lW "$LIBC" | grep -c INTERP || true)
        [ "$interp" = 0 ] && ok "libc.so has no PT_INTERP" || bad "libc.so has a PT_INTERP"
        soname=$($READELF -dW "$LIBC" | grep '(SONAME)' | sed 's/.*\[\(.*\)\]/\1/')
        [ "$soname" = libc.so ] && ok "SONAME libc.so (what DT_NEEDED asks for)" \
                || bad "SONAME is '$soname'"
        for s in __mlibc_entry printf __do_syscall6 malloc; do
                $READELF --dyn-syms -W "$LIBC" | awk -v sym="$s" '$8 == sym { found = 1 } END { exit !found }' \
                        && ok "libc.so exports $s" || bad "libc.so does not export $s"
        done

        echo "== libc.so under disassembly (the library calls now live here) =="

        # write / execve / the restorer: the calls that went through the
        # static libc.a in step 4 -- now in the shared library ld.so maps.
        for n in 1 7 12; do
                if reaches_file "$n" "$LIBC"; then
                        ok "$(scname "$n") reaches syscall $n"
                else
                        bad "no syscall $n ($(scname "$n")) in libc.so"
                fi
        done

        echo "== crt and library =="

        for o in Scrt1.o crti.o crtn.o libc.a libc.so ld.so; do
                if [ -n "${2:-}" ] && [ -f "$2/usr/lib/$o" ]; then
                        ok "$2/usr/lib/$o installed"
                else
                        bad "$2/usr/lib/$o missing"
                fi
        done

        if [ "$fail" = 0 ]; then
                echo "verify: all checks passed"
        else
                echo "verify: $fail check(s) failed"
                exit 1
        fi
        exit 0
fi

echo "== ELF shape (what the loader checks) =="

type=$($READELF -h "$BIN" | sed -n 's/.*Type:.*\(DYN\|EXEC\).*/\1/p')
[ "$type" = DYN ] && ok "ET_DYN (static PIE)" || bad "ELF type is $type, wanted DYN"

interp=$($READELF -lW "$BIN" | grep -c INTERP || true)
[ "$interp" = 0 ] && ok "no PT_INTERP" || bad "has a PT_INTERP"

dyn=$($READELF -lW "$BIN" | grep -c ' DYNAMIC ' || true)
[ "$dyn" -ge 1 ] && ok "has PT_DYNAMIC (the loader's DT_RELA source)" \
        || bad "no PT_DYNAMIC: the loader cannot find the relocations"

needed=$($READELF -dW "$BIN" | grep -c '(NEEDED)' || true)
[ "$needed" = 0 ] && ok "no DT_NEEDED" || bad "has DT_NEEDED entries"

jmprel=$($READELF -dW "$BIN" | grep -c '(JMPREL)' || true)
[ "$jmprel" = 0 ] && ok "no DT_JMPREL (the loader refuses PLT relocations)" \
        || bad "has DT_JMPREL (PLT relocations)"

irel=$($READELF -rW "$BIN" | grep -c 'IRELATIVE' || true)
[ "$irel" = 0 ] && ok "no IRELATIVE (no IFUNCs: the port builds them off)" \
        || bad "has IRELATIVE relocations (an IFUNC slipped in)"

relasz=$($READELF -dW "$BIN" | awk '/\(RELASZ\)/ {gsub(/[^0-9]/, "", $3); print $3}' | head -1)
[ -n "$relasz" ] && [ "$relasz" -gt 0 ] \
        && ok "DT_RELASZ = $relasz (the RELATIVE relocation table)" \
        || bad "no relocation table (DT_RELASZ missing or zero)"

nrel=$($READELF -rW "$BIN" | grep -c 'R_X86_64_RELATIVE' || true)
[ "$nrel" -gt 0 ] && ok "$nrel R_X86_64_RELATIVE relocations" \
        || bad "no R_X86_64_RELATIVE relocations"

# The loader's own geometry rule (elf.rs): segments may sit anywhere but no
# two may share a page -- each page carries one protection.  That is
# page_down(next.vaddr) >= page_up(prev.vaddr + prev.memsz), the rule that
# made the kernel ask for -z max-page-size=4096 in the first place.
shared=0
prev_v= prev_m=
while read -r v m; do
        v=$((0x${v#0x})); m=$((0x${m#0x}))
        if [ -n "$prev_v" ]; then
                e=$((prev_v + prev_m + 4095))
                prev_end=$((e - e % 4096))
                down=$((v - v % 4096))
                [ "$down" -lt "$prev_end" ] && shared=$((shared + 1))
        fi
        prev_v=$v
        prev_m=$m
done <<EOF
$($READELF -lW "$BIN" | awk '/^ *LOAD/ {print $3, $6}')
EOF
[ "$shared" = 0 ] \
        && ok "no two segments share a page (one protection per page, W^X)" \
        || bad "$shared segment pair(s) share a page"

exec_stack=$($READELF -lW "$BIN" | grep -c 'GNU_STACK.*RWE' || true)
[ "$exec_stack" = 0 ] && ok "non-executable stack" || bad "executable stack"

echo "== syscalls under disassembly =="

# The per-program expectations: every syscall the program itself makes,
# and the symbols those paths live in.  Without a program argument the
# step-3 proof set applies (write, the restorer, clock_gettime).
case "$PROG" in
hello)  SYSCALLS="1 7 3 4 5"
        SYMBOLS="write getauxval" ;;
fork)   SYSCALLS="1 8 9 10 38 7 5"
        SYMBOLS="write fork waitpid signal kill __mlibc_signal_restore" ;;
fd)     SYSCALLS="1 13 14 15 16 17 18 8 9 7 38 5"
        SYMBOLS="write read open close fork __mlibc_signal_restore" ;;
posix)  SYSCALLS="1 13 14 15 16 8 9 10 7 19 20 21 22 23 24 25 26 27 28 29 30 31 32 33 34 35 38"
        SYMBOLS="write stat fstat mmap opendir readdir uname tcgetattr __mlibc_signal_restore" ;;
"")     SYSCALLS="1 12 43"
        SYMBOLS="write __mlibc_signal_restore clock_gettime" ;;
*)      echo "verify: unknown program '$PROG' (hello | fork | fd | posix)" >&2; exit 2 ;;
esac

for n in $SYSCALLS; do
        if reaches "$n"; then
                ok "$(scname "$n") reaches syscall $n"
        else
                bad "no syscall $n ($(scname "$n"))"
        fi
done

# The symbols those paths live in.
for s in $SYMBOLS; do
        if $NM "$BIN" 2>/dev/null | grep -q " $s$"; then
                ok "symbol $s present"
        else
                bad "symbol $s missing"
        fi
done

echo "== crt and library =="

for o in Scrt1.o crti.o crtn.o libc.a; do
        if [ -n "${2:-}" ] && [ -f "$2/usr/lib/$o" ]; then
                ok "$2/usr/lib/$o installed"
        else
                bad "$2/usr/lib/$o missing"
        fi
done

if [ "$fail" = 0 ]; then
        echo "verify: all checks passed"
else
        echo "verify: $fail check(s) failed"
        exit 1
fi
