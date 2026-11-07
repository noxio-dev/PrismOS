# rootfs/ -- the editable part of the root file system

Everything under this directory is copied verbatim into the image's root
(`/`) by `make root`, before the packages are installed on top of it.  It
holds the files you edit by hand: `/etc/passwd`, `/etc/profile`, `/etc/rc`
(the start-up script init runs), `/etc/rc.d/`, ...

- Add a file here and the next `make` puts it in the image, at the same path.
- Scripts start with `#!/bin/sh` and need the executable bit (`chmod +x`).
- Directories that must exist but stay empty (`/dev`, `/proc`, `/tmp`) and
  the usr-merge symlinks are created by the Makefile, not stored here.
- Compiled programs do not belong here: they are packages (`pkg/`), built
  and installed by `./prism`.
