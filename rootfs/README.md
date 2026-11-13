# rootfs/ -- the editable part of the root file system

Everything under this directory is copied verbatim into the image's root
(`/`) by `make root`, before the packages are installed on top of it.  It
holds the files you edit by hand: `/etc/passwd`, `/etc/profile`, `/etc/rc`
(the start-up script init runs), `/etc/rc.d/`, ...

- Add a file here and the next `make` puts it in the image, at the same path.
- Scripts start with `#!/bin/sh` and need the executable bit (`chmod +x`).
- The pid-1 mounts (`/proc`, `/tmp`) are made by `/etc/rc.d/S00mounts` at
  boot, through mount(2) -- the kernel keeps `/` and `/dev`; a start-up
  script that needs them runs after that one (S10name and up).  Nothing
  else in the boot path mounts a file system: keep it that way, `ps`, `df`
  and `mount` all read `/proc/mounts`.
- Directories that must exist but stay empty (`/dev`, `/proc`, `/tmp`) and
  the usr-merge symlinks are created by the Makefile, not stored here.
- Compiled programs do not belong here: they are packages (`pkg/`), built
  and installed by `./prism`.

## Keyboard

The kernel boots with the `us` keymap.  `/etc/rc.d/S10keymap` loads
`/usr/share/keymaps/$(cat /etc/keymap).bkmap` with BusyBox `loadkmap`: write
`fr` in `/etc/keymap` for AZERTY.  `dumpkmap > file` saves the current map,
`kbd_mode`, `showkey` and the kernel shell command `kbd` inspect it.  The built-in maps are
regenerated with `tools/mkkmap.rs`; the decoder is tested on the host with
`tests/keymap_host.rs`.
