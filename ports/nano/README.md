# nano — a full-screen editor on the console (phase 4.2)

```
ports/nano/
├── build.sh                       the port: fetch → the prism-cc driver →
│                                  ncurses → nano → stage (stripped, the
│                                  cross strip; the terminfo, the host tic)
│                                  → verify (verify.sh custom + prism_emu)
├── prism.src                      the TERM=prism terminfo source — what
│                                  console/ansi.rs really implements
└── patches/
    ├── ncurses-6.6-prismos.patch  the triplet in config.sub, plus the
    │                              unconditional search.h block of
    │                              new_pair.c (see below)
    └── nano-9.3-prismos.patch     the triplet in config.sub
```

Phase 4.2's sentence, verbatim from the roadmap's spirit: *a full-screen
editor in the image*.  nano cannot draw by itself — the curses layer is
part of the port: **ncurses 6.6 builds first** (`--enable-widec`,
`--without-progs`, static `libncursesw.a` only: with_termlib defaults to
no in 6.6, so the terminfo core — tgetent, setupterm, tigetstr — is
archived inside the one library), installing into the port's **own
sysroot** `build/nano-native/sysroot` so the mlibc sysroot stays pristine
(the way a toolchain rebuild would wipe a stranger's headers).  Then
**nano 9.3** against it, `--disable-nls --disable-rpath
--disable-speller`, one binary out: `build/nano-out/nano`, stripped under
the kernel's 4 MiB whole-file read (~1.4 MiB — the terminfo tables and
gnulib inflate; busybox's ~2.5 MiB put this in scale).

**TERM=prism.**  The kernel env already spells it (`sys/kern/src/main.rs`);
this port is why it exists.  `ports/nano/prism.src` describes the console
the ANSI interpreter (`sys/kern/src/console/ansi.rs`) made of tty0: the
VT102 subset it answers (cursor addressing, CUU/CUD/CUF/CUB, erases,
index/reverse-index, DECSTBM regions, the save/restore pair, cursor
visibility), its SGR colors (16 both planes, bold/dim/reverse), and the
escape strings the keymap sends (Linux's defkeymap arrows and
Home/End/Insert/Delete/PgUp/PgDn/F1..F12 — so no smkx transmit mode is
needed).  `am` without `xenl` matches the DECAWM wrap-pending discipline
of the VGA and framebuffer consoles; `bce` is honest — both consoles
erase in the current background.  The **host** `tic` compiles it (the
on-disk format is stable; the target builds no progs) into
`build/nano-out/terminfo/p/prism`, which `make root` copies to
`/usr/share/terminfo/p/prism` — the layout ncurses looks up first.

**the driver.**  the binutils port's `prism-cc`, generated at
`build/nano-native/prism-cc`, with one answer added: an `-l` that
resolves nowhere in the `-L` list falls back to the mlibc sysroot's own
archives (`libm.a` exists there — mlibc ships one — and ncurses' and
nano's configure probes spell `-lm` with no `-L`).  Archives and
resolved libraries ride the same `--start-group` as libc.a, so link
order never matters; the static-PIE sandwich is exactly
`tools/prism.py`'s.

Two upstream accommodations ride in the ncurses patch: the triplet in
`config.sub`, and `new_pair.c`'s `#include <search.h>` block made
unconditional — upstream rides it behind `NCURSES_EXT_COLORS` (off
here), but the file calls `tsearch`/`tdelete` regardless; gcc passes the
implicit declarations as warnings, clang's int-to-pointer conversions
are errors.  The renames (`entry`→`my_entry`) travel with the include
because mlibc's search.h names the tsearch node `struct entry`, the same
name ncurses' terminfo entry carries (new_pair.c itself only mentions
entry in comments).  `-D_GNU_SOURCE` rides the curses CFLAGS for the
same reason: mlibc keeps tsearch behind it.

**verify.**  `ports/toolchain/verify.sh`'s custom mode — the loader's
whole static-PIE contract, plus the symbols the UTF-8 path rides
(`mbstowcs`, `wcrtomb` — what turns a typed é into the codepoint
ncursesw places as one glyph; `getopt_long`/`regcomp` never appear, the
gnulib layer renames what it replaces to `rpl_*`) — then the functional
smoke: `nano --version` under `prism_emu`, the repo's ptrace runner,
compiled from the repo root on the spot (the runner's argv spelling is
exec's: the first extra argument is the guest argv[0], so the ELF path
travels twice).

**accents.**  mlibc's `MB_CUR_MAX` is 4 and its transcoders decode UTF-8
-- but `current_charcode()` returned the ASCII charcode for the default
C locale (codeset `ANSI_X3.4-1968`), and mlibc's real locale system
cannot load a UTF-8 locale without a locale database, which the image
does not ship.  The port's contract fixes it in kind: the charcode is
UTF-8, unconditionally (the patch lives in `ports/mlibc`), because the
PrismOS console IS UTF-8 — unifont, one glyph per codepoint — and the
keymap already produces it (vi's 8-bit input rode the same decision).

The Makefile's root rule copies the binary and the terminfo into the
image: `nano` (the `make nano` target runs this script), then
`make` repacks `build/prismos.img`.
