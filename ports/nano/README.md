# nano — a full-screen editor on the console (phase 4.2)

```
ports/nano/
├── build.sh                       the port: fetch → the prism-cc driver →
│                                  ncurses (both models) → nano (twice) →
│                                  stage (stripped, the cross strip; the
│                                  terminfo, the host tic) → verify
│                                  (custom + dyn + prism_emu + closure)
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
part of the port, and it ships **dynamic** — the image's first shared
library beside libc.so.  **ncurses 6.6 builds both models in one
configure** (`--enable-widec --with-shared --with-normal`,
`--without-progs`: with_termlib defaults to no in 6.6, so the terminfo
core — tgetent, setupterm, tigetstr — lives inside the one library either
way): `libncursesw.a` for the static twin, and the DSO
`libncursesw.so.6.6` — SONAME `libncursesw.so.6`, what a DT_NEEDED
record says and ld.so looks up in `/usr/lib` at run time — for the
image.  The shared model's `-fPIC` rides the CFLAGS globally, because
ncurses' CC_SHARED_OPTS probe only answers for gcc-like compilers
(`GCC=no`: the driver is clang behind a wrapper) and `*linux*` system
names (`cf_cv_system_name` is `x86_64-prismos`) — the generated
configure's case statements are taught the triplet at fetch (see below).
The libraries install into the port's **own sysroot**
`build/nano-native/sysroot` so the mlibc sysroot stays pristine (the way
a toolchain rebuild would wipe a stranger's headers).  Then **nano 9.3**
against it, `--disable-nls --disable-rpath --disable-speller`, **twice**:

- `build/nano-out/nano` — a **dynamic PIE**: PT_INTERP `/lib/ld.so`,
  DT_NEEDED `libc.so` + `libncursesw.so.6`, `-z now` (BIND_NOW — the
  rtld zeroes GOT[1]/GOT[2] and cannot lazy-bind), stripped under the
  kernel's 4 MiB whole-file read.  This is what `make root` copies into
  the image: the editor into `/usr/bin`, the stripped DSO — its file
  name exactly its SONAME — beside ld.so and libc.so into `/usr/lib`,
  where the rtld's default paths look.
- `build/nano-out/nano-static` — a static twin, the same link
  `tools/prism.py` makes for every app, ncursesw welded in.  **Not
  shipped**: it is the smoke vehicle, because `prism_emu` only
  implements the static-PIE contract.

The reference for the dynamic link is `do_dyn` of `ports/toolchain`
(the phase-3.2 proof executable); the kernel is unchanged — ld.so maps
the second DSO with the same syscalls it maps libc.so with, and looks
it up in the default paths `/lib`, `/lib64`, `/usr/lib`, `/usr/lib64`.

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
`build/nano-native/prism-cc`, with **three link answers** now:

- `-shared` — a **DSO**: `ld -shared`, no crt, no entry, no libgcc; `-lc`
  resolves to the mlibc sysroot's `libc.so` so the record is DT_NEEDED
  `libc.so` (the basename ld.so looks for); `-z max-page-size=4096
  -z separate-code` keep one protection per page the way ld.so maps DSO
  segments; `-Wl,a,b,c` reaches ld comma-split, verbatim — except
  `-stats`, an AIX-ism the GNU ld does not know.
- `PRISM_LINK_MODE=dyn` — the **dynamic PIE**: exactly `do_dyn`'s recipe
  (PT_INTERP `/lib/ld.so`, `-z now`, the dynamic crt sandwich, libc.so in
  the group), with `-rpath-link` over the two sysroots so ld can trace
  DT_NEEDED chains.
- the default — the **static sandwich** the kernel's loader applies
  itself (the twin).

An `-l` resolves in the port's sysroot first (the ncurses the port
installed), then the `-L` list, then the mlibc sysroot's own (`libm.a`
exists there — mlibc ships one — and ncurses' and nano's configure probes
spell `-lm` with no `-L`); the dynamic modes prefer the shared object
(`lib<name>.so`, then `lib<name>.so.*`) over the archive — the DSO link
has no archive fallback at all.  Archives and resolved libraries ride
the same `--start-group` as libc.a in the static answer, so link order
never matters there.

One more fetch-time accommodation: the **generated** configure's case
statements for shared-library systems spell linux / gnu / k\*bsd /
freebsd only — an unmatched `cf_cv_system_name` leaves `CC_SHARED_OPTS`
`unknown` and the shared model errors out.  Two statements carry the
pattern (the `LD_RPATH_OPT` one and the `MK_SHARED_LIB` one) and are
taught the triplet with a sed at fetch, the way `config.sub` is taught
by the patch; the shared recipe they spell is exactly the one the
driver answers: `-shared -Wl,-soname,<soname>,-stats,-lc`.

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

**verify.**  three layers:

1. the **static twin** through `ports/toolchain/verify.sh`'s custom mode
   — the loader's whole static-PIE contract, plus the symbols the UTF-8
   path rides (`mbstowcs`, `wcrtomb` — what turns a typed é into the
   codepoint ncursesw places as one glyph; `getopt_long`/`regcomp` never
   appear, the gnulib layer renames what it replaces to `rpl_*`) — then
   the functional smoke: `nano --version` under `prism_emu`, the repo's
   ptrace runner, compiled from the repo root on the spot (the runner's
   argv spelling is exec's: the first extra argument is the guest
   argv[0], so the ELF path travels twice).
2. the **dynamic couple** through the dyn mode, which this port extends
   with knobs (`PRISM_VERIFY_NEEDED` — two DT_NEEDED records instead of
   hello's one; `PRISM_VERIFY_RAW=0` — nano does not ride `prism_raw.h`;
   `PRISM_VERIFY_DSO`/`DSO_SONAME`/`DSO_SYMBOLS` — the ncurses DSO
   checked the way ld.so is: ET_DYN, no PT_INTERP, its SONAME, its
   RELATIVE relocations, and the exports the UTF-8 editor calls:
   `initscr endwin tgetent setupterm`).
3. the **symbol closure**, checked statically: every strong undefined of
   the executable must be answered by the executable itself, libc.so or
   the ncurses DSO, and the DSO's by itself or libc.so — the promise the
   rtld would otherwise only keep at run time.

**accents.**  mlibc's `MB_CUR_MAX` is 4 and its transcoders decode UTF-8
-- but `current_charcode()` returned the ASCII charcode for the default
C locale (codeset `ANSI_X3.4-1968`), and mlibc's real locale system
cannot load a UTF-8 locale without a locale database, which the image
does not ship.  The port's contract fixes it in kind: the charcode is
UTF-8, unconditionally (the patch lives in `ports/mlibc`), because the
PrismOS console IS UTF-8 — unifont, one glyph per codepoint — and the
keymap already produces it (vi's 8-bit input rode the same decision).

The Makefile's root rule copies the dynamic couple and the terminfo into
the image: `nano` + `libncursesw.so.6` + the `prism` entry (the `make
nano` target runs this script), then `make` repacks `build/prismos.img`.
The twin (`nano-static`) stays in `build/nano-out/` only.
