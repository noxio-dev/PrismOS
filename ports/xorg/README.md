# Xorg — step 1: util-macros, xorgproto, xtrans

```
ports/xorg/
├── build.sh    fetch → macros → proto → xtrans → verify
└── README.md
```

Step 1 of `docs/XORG.md` section 3 ("Userspace ports"): the three packages
every later X port configures against.  None of them compiles anything for
the target:

| package | what it installs |
|---------|------------------|
| `util-macros` | `share/aclocal/xorg-macros.m4`, `xorg-macros.pc` |
| `xorgproto` | the protocol headers (`X11/X.h`, `X11/Xproto.h`, `X11/extensions/*.h`) and the `xproto.pc`, `xextproto.pc`, `randrproto.pc` ... files |
| `xtrans` | the transport layer as sources + headers (`X11/Xtrans/*`) and `xtrans.pc` |

They are architecture-independent, so they are configured **without
`--host`**, with the host's own tools — no `config.sub` patch, and no
dependency on `make mlibc`.  They land in the port's own sysroot,
`build/xorg-native/sysroot`, with `--prefix=/usr` + `DESTDIR` (the mlibc
sysroot stays pristine, as in `ports/nano`).  Nothing goes into the image:
headers and macros are build-time only.

## Usage

```
sh ports/xorg/build.sh                 # every stage
sh ports/xorg/build.sh stage proto     # one stage (fetch macros proto xtrans verify)
make xorg                              # same thing, with a stamp
```

Versions and mirrors are overridable: `UTIL_MACROS_VER` (1.20.2),
`XORGPROTO_VER` (2024.1), `XTRANS_VER` (1.6.0), `XORG_MIRROR`
(`xorg.freedesktop.org`, then `www.x.org` as a fallback).  A tarball
already present in `build/xorg-src` (`.tar.xz` or `.tar.gz`) is used as it
is, so the tarballs can be dropped there to build offline.

Host requirements: a C compiler, make, pkg-config, curl, tar + xz.
autoconf / automake are not needed (the tarballs ship a generated
`configure`).

## What `verify` checks

- the headers and `xorg-macros.m4` are in the sysroot;
- `pkg-config`, pointed at the sysroot the cross way
  (`PKG_CONFIG_SYSROOT_DIR=$SYSROOT`,
  `PKG_CONFIG_LIBDIR=$SYSROOT/usr/share/pkgconfig:$SYSROOT/usr/lib/pkgconfig`),
  resolves `xproto xextproto randrproto kbproto inputproto xtrans xorg-macros`;
- a host compiler parses `X11/Xproto.h` and `X11/extensions/randr.h` from
  the sysroot alone (`-fsyntax-only`).

The later ports use the same two variables, plus
`ACLOCAL_PATH=$SYSROOT/usr/share/aclocal` for anything that runs
`autoreconf`.

## Status

- `xorgproto` 2024.1: built and verified from the release tarball
  (`stage proto`, then `stage verify`: `xproto` 7.0.33, `xextproto` 7.3.0,
  `randrproto` 1.6.0, `kbproto`, `inputproto`; the headers compile from
  the sysroot).
- `util-macros` and `xtrans`: not run yet (no tarball at hand); `verify`
  reports them as skipped until they are installed.  Neither is needed to
  build `xorgproto` (its tarball ships its own `configure` and
  `aclocal.m4`).

Each stage fetches its own package, so `stage proto` works with only
`xorgproto-2024.1.tar.xz` **or** `.tar.gz` in `build/xorg-src`.  The
stage logs land in `build/xorg-native/obj/<package>/`.
