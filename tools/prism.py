#!/usr/bin/env python3
#-
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (c) 2026 noxio-dev
#
# tools/prism.py -- the PrismOS package tool.
#
# A userland program on PrismOS is a *package*: a directory under apps/ with
# a prism.toml manifest.  prism reads the manifest and does everything the
# way the kernel's ELF loader (sys/kern/src/user/elf.rs) wants it:
#
#   build    the freestanding compile against the mlibc sysroot (the
#            x86_64-prismos cross tools of build/toolchain/usr), then the
#            static-PIE link -- Scrt1 / crti / crtbeginS / the program /
#            libc.a / crtendS / crtn, the sandwich a -static-pie driver
#            assembles, with the segment geometry the loader requires
#            (-z separate-code -z max-page-size=4096)
#   verify   ports/toolchain/verify.sh against the built binary: the
#            loader-contract shape checks, plus the program's own syscall
#            and symbol diet when the manifest declares one ([verify])
#   stage    the stripped binary into build/apps-out/<name> -- the whole
#            file the kernel's exec reads must stay under MAX_ELF_FILE
#            (4 MiB), and the meson debug sections buy the loader nothing
#   install  the staged binary into an image root tree, at [install] path
#
# Usage (or, the same through the ./prism shim at the repo root):
#   python3 tools/prism.py init <name>     scaffold apps/<name>
#   python3 tools/prism.py build [name]    build one app, or all of them
#   python3 tools/prism.py install [name]  install into a root tree
#   python3 tools/prism.py verify [name]   the contract checks, alone
#   python3 tools/prism.py list            the apps and their manifests
#   python3 tools/prism.py run             make the image and boot it
#   python3 tools/prism.py clean [name]    remove build outputs
#
# Python 3.8+ -- the manifest reader uses tomllib when the host has it
# (3.11+) and a small TOML subset parser otherwise.
import glob
import os
import re
import shutil
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
APPS_DIR = os.path.join(REPO, "apps")
BUILD_DIR = os.path.join(REPO, "build")
PREFIX = os.path.join(BUILD_DIR, "toolchain", "usr")
SYSROOT = os.path.join(PREFIX, "x86_64-prismos")
CC = os.path.join(PREFIX, "bin", "x86_64-prismos-gcc")
LD = os.path.join(PREFIX, "bin", "x86_64-prismos-ld")
STRIP = os.path.join(PREFIX, "bin", "x86_64-prismos-strip")
VERIFY_SH = os.path.join(REPO, "ports", "toolchain", "verify.sh")
SHARED_HEADERS = [os.path.join(APPS_DIR, "include", "prism_raw.h")]

# The do_prog discipline of ports/toolchain/bootstrap.sh, verbatim.
BASE_CFLAGS = [
    "-O2", "-Wall", "-Wextra", "-ffreestanding", "-fPIE",
    "-fno-stack-protector", "-fno-asynchronous-unwind-tables",
    "-fno-math-errno",
]
BASE_LDFLAGS = [
    "-m", "elf_x86_64", "-pie", "--no-dynamic-linker",
    "-z", "separate-code", "-z", "max-page-size=4096",
    "-z", "noexecstack", "-z", "norelro", "-e", "_start",
]


def die(msg):
    sys.exit("prism: error: " + msg)


def say(app, msg):
    print("  %-10s %s" % (app, msg))


# --------------------------------------------------------------------- TOML
# tomllib on 3.11+, a subset parser otherwise: sections, strings, integers,
# booleans, single-line arrays, # comments (kept out of quoted strings).
def _strip_comment(line):
    out, quote = [], None
    for ch in line:
        if quote:
            out.append(ch)
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            out.append(ch)
        elif ch == "#":
            break
        else:
            out.append(ch)
    return "".join(out)


def _unquote(s):
    if len(s) < 2 or s[0] not in "\"'" or s[-1] != s[0]:
        raise ValueError("not a quoted string: %s" % s)
    body = s[1:-1]
    for esc, real in (("\\\\", "\\"), ("\\\"", '"'), ("\\n", "\n"), ("\\t", "\t")):
        body = body.replace(esc, real)
    return body


def _split_array(body):
    items, cur, quote = [], "", None
    for ch in body:
        if quote:
            cur += ch
            if ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
            cur += ch
        elif ch == ",":
            items.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        items.append(cur.strip())
    return [i for i in items if i]


def _parse_scalar(s):
    if s in ("true", "false"):
        return s == "true"
    if re.fullmatch(r"[+-]?[0-9]+", s):
        return int(s)
    if s and s[0] in "\"'":
        return _unquote(s)
    raise ValueError("unsupported value: %s" % s)


def _parse_toml_subset(text):
    data, section = {}, None
    for lineno, raw in enumerate(text.splitlines(), 1):
        line = _strip_comment(raw).strip()
        if not line:
            continue
        try:
            if line.startswith("[") and line.endswith("]"):
                name = line[1:-1].strip()
                if not name or "." in name:
                    raise ValueError("section %s" % line)
                section = data.setdefault(name, {})
            else:
                key, _, val = line.partition("=")
                if section is None or not key.strip() or not _:
                    raise ValueError(line)
                val = val.strip()
                if val.startswith("["):
                    if not val.endswith("]"):
                        raise ValueError("array must fit on one line")
                    value = [_parse_scalar(i) for i in _split_array(val[1:-1])]
                else:
                    value = _parse_scalar(val)
                section[key.strip()] = value
        except ValueError as e:
            raise ValueError("line %d: %s" % (lineno, e))
    return data


def load_toml(path):
    with open(path, "r", encoding="utf-8") as f:
        text = f.read()
    try:
        import tomllib  # Python 3.11+
        return tomllib.loads(text)
    except ImportError:
        return _parse_toml_subset(text)


# ----------------------------------------------------------------- package
class ManifestError(Exception):
    pass


class App:
    def __init__(self, directory):
        self.dir = directory
        self.manifest = os.path.join(directory, "prism.toml")
        data = load_toml(self.manifest)

        pkg = data.get("package", {})
        build = data.get("build", {})
        install = data.get("install", {})
        verify = data.get("verify", {})

        self.name = pkg.get("name") or ""
        self.version = str(pkg.get("version", ""))
        self.description = pkg.get("description", "")

        self.makefile = build.get("makefile")
        self.target = build.get("target", "all")
        self.sources = build.get("sources", [])
        self.cflags = build.get("cflags", [])
        self.ldflags = build.get("ldflags", [])
        self.output = build.get("output", os.path.join("build", self.name or "app"))
        self.strip = build.get("strip", True)
        self.want_verify = build.get("verify", True)
        self.syscalls = [str(n) for n in verify.get("syscalls", [])]
        self.symbols = list(verify.get("symbols", []))

        # identity: the directory under apps/ is the package's name
        dirname = os.path.basename(os.path.normpath(directory))
        if self.name != dirname:
            raise ManifestError("package name %r must match its directory %r"
                                % (self.name, dirname))
        if not self.version:
            raise ManifestError("[package] version is required")

        if self.makefile:
            if self.sources:
                raise ManifestError("[build] makefile and sources are exclusive")
        else:
            if not self.sources:
                raise ManifestError("[build] needs sources (or a makefile)")

        self.install_path = install.get("path", "")
        if not self.install_path.startswith("/"):
            raise ManifestError("[install] path must be absolute, got %r"
                               % self.install_path)

        for field, values in (("sources", self.sources), ("cflags", self.cflags),
                              ("ldflags", self.ldflags)):
            if not isinstance(values, list) or not all(isinstance(v, str) for v in values):
                raise ManifestError("[build] %s must be an array of strings" % field)
        for field, values in (("syscalls", verify.get("syscalls", [])),
                              ("symbols", self.symbols)):
            if not isinstance(values, list):
                raise ManifestError("[verify] %s must be an array" % field)

    # -- paths -----------------------------------------------------------
    def app_rel(self, path):
        """Resolve an app-relative path, refusing to escape the app dir."""
        full = os.path.normpath(os.path.join(self.dir, path))
        if not (full == self.dir or full.startswith(self.dir + os.sep)):
            raise ManifestError("path escapes the package: %s" % path)
        return full

    @property
    def output_path(self):
        return self.app_rel(self.output)

    @property
    def staged(self):
        return os.path.join(BUILD_DIR, "apps-out", self.name)

    def source_files(self):
        if self.makefile:
            return []
        out = []
        for pattern in self.sources:
            hits = sorted(glob.glob(os.path.join(self.dir, pattern)))
            if not hits:
                raise ManifestError("no source matches %r" % pattern)
            out.extend(hits)
        return out

    def build_deps(self):
        deps = [self.manifest] + self.source_files() + SHARED_HEADERS
        if self.makefile:
            deps.append(self.app_rel(self.makefile))
        return deps

    def is_stale(self):
        if not os.path.exists(self.output_path):
            return True
        mtime = os.path.getmtime(self.output_path)
        return any(os.path.getmtime(d) > mtime for d in self.build_deps()
                   if os.path.exists(d))


def find_apps():
    if not os.path.isdir(APPS_DIR):
        return []
    apps = []
    for manifest in sorted(glob.glob(os.path.join(APPS_DIR, "*", "prism.toml"))):
        try:
            apps.append(App(os.path.dirname(manifest)))
        except (ManifestError, ValueError) as e:
            die("%s: %s" % (manifest, e))
    return apps


def get_app(name):
    for app in find_apps():
        if app.name == name:
            return app
    die("no package %r (one of: %s)" %
        (name, " ".join(a.name for a in find_apps()) or "none"))


# ------------------------------------------------------------------- build
def check_toolchain():
    if not os.path.exists(CC):
        die("no toolchain in %s -- run `make` (or `make mlibc`) first" % PREFIX)
    if not os.path.exists(os.path.join(SYSROOT, "usr", "lib", "libc.a")):
        die("no libc.a in the sysroot -- run `make mlibc` first")


def gcc_include_dir():
    return subprocess.check_output([CC, "-print-file-name=include"],
                                   text=True).strip()


def compile_and_link(app):
    if app.makefile:
        makefile = app.app_rel(app.makefile)
        subprocess.check_call(["make", "-C", app.dir, "-f",
                              os.path.basename(makefile), app.target])
        if not os.path.exists(app.output_path):
            die("%s: the build produced no %s" % (app.name, app.output))
        return

    objects = []
    for src in app.source_files():
        if not src.endswith(".c"):
            die("%s: only .c sources are built in builtin mode (%s)"
                % (app.name, src))
        stem = os.path.splitext(src)[0][len(app.dir) + 1:].replace(os.sep, "_")
        obj = app.app_rel(os.path.join("build", "obj", stem + ".o"))
        os.makedirs(os.path.dirname(obj), exist_ok=True)
        cmd = [CC, "-c", src, "-o", obj] + BASE_CFLAGS + [
            "-nostdinc",
            "-isystem", gcc_include_dir(),
            "-isystem", os.path.join(SYSROOT, "usr", "include"),
            "-I", os.path.join(APPS_DIR, "include"),
        ] + app.cflags
        say(app.name, "compiling %s" % os.path.relpath(src, app.dir))
        subprocess.check_call(cmd)
        objects.append(obj)

    crtbegin = subprocess.check_output(
        [CC, "-print-file-name=crtbeginS.o"], text=True).strip()
    crtend = subprocess.check_output(
        [CC, "-print-file-name=crtendS.o"], text=True).strip()
    libdir = os.path.join(SYSROOT, "usr", "lib")

    def link(extra_group):
        return [LD] + BASE_LDFLAGS + app.ldflags + [
            "-o", app.output_path,
            os.path.join(libdir, "Scrt1.o"), os.path.join(libdir, "crti.o"),
            crtbegin,
        ] + objects + [
            "--start-group", os.path.join(libdir, "libc.a"),
        ] + extra_group + [
            "--end-group",
            crtend, os.path.join(libdir, "crtn.o"),
        ]

    os.makedirs(os.path.dirname(app.output_path), exist_ok=True)
    say(app.name, "linking the static PIE")
    try:
        subprocess.check_call(link([]))
    except subprocess.CalledProcessError:
        # the do_prog retry: compiler intrinsics (__divti3 and friends) pull
        # libgcc in -- the sandwich's group swallows it
        libgcc = subprocess.check_output(
            [CC, "-print-libgcc-file-name"], text=True).strip()
        say(app.name, "retrying with libgcc (compiler intrinsics)")
        subprocess.check_call(link([libgcc]))


def run_verify(app):
    if not app.want_verify:
        say(app.name, "verify skipped (verify = false)")
        return
    env = dict(os.environ)
    env["PRISM_VERIFY_SYSCALLS"] = " ".join(app.syscalls)
    env["PRISM_VERIFY_SYMBOLS"] = " ".join(app.symbols)
    # custom mode: the shape checks always, the manifest's diet on top
    subprocess.check_call(["sh", VERIFY_SH, app.output_path, SYSROOT, "custom"],
                         env=env)


def stage(app):
    """build/apps-out/<name>: the stripped binary the image installs."""
    os.makedirs(os.path.dirname(app.staged), exist_ok=True)
    if app.strip:
        subprocess.check_call([STRIP, "--strip-all", "-o", app.staged,
                               app.output_path])
    else:
        shutil.copy2(app.output_path, app.staged)
    kib = (os.path.getsize(app.staged) + 1023) // 1024
    say(app.name, "staged %s (%d KiB)" % (os.path.relpath(app.staged, REPO), kib))


def build(app, force=False):
    if force or app.is_stale():
        compile_and_link(app)
        run_verify(app)
        stage(app)
        say(app.name, "built %s" % os.path.relpath(app.output_path, REPO))
    elif not os.path.exists(app.staged) or \
            os.path.getmtime(app.staged) < os.path.getmtime(app.output_path):
        run_verify(app)
        stage(app)
        say(app.name, "restaged (binary was up to date)")
    else:
        say(app.name, "up to date")


# ----------------------------------------------------------------- install
def install(app, dest):
    src = app.staged if os.path.exists(app.staged) else None
    if src is None or (os.path.exists(app.output_path)
                       and os.path.getmtime(app.output_path) > os.path.getmtime(src)):
        build(app)
        src = app.staged
    dst = os.path.normpath(dest + os.sep + app.install_path)
    if not dst.startswith(os.path.normpath(dest) + os.sep):
        die("%s: install path %s escapes the destination tree"
            % (app.name, app.install_path))
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    shutil.copy2(src, dst)
    os.chmod(dst, 0o755)
    say(app.name, "installed %s" % os.path.relpath(dst, REPO))


# ------------------------------------------------------------------- init
INIT_MAIN_C = """\
/* %(name)s -- a PrismOS app, started from `prism init`.
 * The C library (mlibc, in the sysroot) for the portable surface; the raw
 * shim (apps/include/prism_raw.h) for the calls it has no word for. */
#include <stdio.h>
#include "prism_raw.h"

int main(void)
{
        printf("hello from %(name)s (uptime %%ld ms)\\n", prism_uptime_ms());
        return 0;
}
"""

INIT_TOML = """\
# The manifest of a PrismOS app -- read by tools/prism.py.  This one builds
# in the builtin mode: prism compiles the sources against the mlibc sysroot
# and links the static PIE the kernel's loader expects, then verify.sh
# checks the loader contract, then the stripped binary is staged for the
# image.  See apps/README.md for every field.
[package]
name = "%(name)s"
version = "0.1.0"
description = "a PrismOS app"

[build]
sources = ["src/main.c"]
# cflags = []                      # extra compile flags
# ldflags = []                     # extra link flags
# output = "build/%(name)s"        # app-relative; where the binary lands
# strip = true                     # the kernel reads whole files (4 MiB cap)
# verify = true                    # verify.sh after every build

[install]
path = "/usr/bin/%(name)s"

# The program's ABI, checked under disassembly: every syscall it makes and
# a symbol or two those paths live in (verify.sh's reaches).  The scaffold
# speaks through printf (syscall 1 at flush time) and the raw uptime clock
# (3); keep the diet honest as you rewrite main.c.
[verify]
syscalls = [1, 3]
symbols = ["printf"]
"""


def cmd_init(args):
    if len(args) != 1:
        die("init needs exactly one name: prism init <name>")
    name = args[0]
    if not re.fullmatch(r"[a-z0-9][a-z0-9-]*", name):
        die("package name %r must be lowercase [a-z0-9-]" % name)
    directory = os.path.join(APPS_DIR, name)
    if os.path.exists(directory):
        die("apps/%s already exists" % name)
    os.makedirs(os.path.join(directory, "src"))
    with open(os.path.join(directory, "prism.toml"), "w") as f:
        f.write(INIT_TOML % {"name": name})
    with open(os.path.join(directory, "src", "main.c"), "w") as f:
        f.write(INIT_MAIN_C % {"name": name})
    print("prism: apps/%s scaffolded -- build it with "
          "`python3 tools/prism.py build %s`" % (name, name))
    print("prism: `make` picks it up: the image installs /usr/bin/%s" % name)


# ----------------------------------------------------------------- commands
def _names(args):
    """The non-flag arguments: package names."""
    return [a for a in args if not a.startswith("-")]


def cmd_build(args):
    check_toolchain()
    names = _names(args)
    for app in ([get_app(n) for n in names] if names else find_apps()):
        build(app)


def cmd_install(args):
    check_toolchain()
    dest = os.path.join(BUILD_DIR, "root")
    names = []
    i = 0
    while i < len(args):
        if args[i] == "--dest":
            if i + 1 >= len(args):
                die("--dest needs a directory")
            dest = args[i + 1]
            i += 2
        elif args[i].startswith("-"):
            i += 1
        else:
            names.append(args[i])
            i += 1
    for app in ([get_app(n) for n in names] if names else find_apps()):
        install(app, dest)


def cmd_verify(args):
    check_toolchain()
    names = _names(args)
    for app in ([get_app(n) for n in names] if names else find_apps()):
        if not os.path.exists(app.output_path) or app.is_stale():
            build(app)          # compiles and verifies
        else:
            run_verify(app)


def cmd_list(_args):
    apps = find_apps()
    if not apps:
        print("prism: no apps (apps/*/prism.toml) -- `prism init <name>`")
        return
    width = max(len(a.name) for a in apps)
    for a in apps:
        mode = "make" if a.makefile else "builtin"
        print("%-*s  %-6s  %-8s  %s" % (width, a.name, mode, a.version,
                                        a.install_path))


def cmd_run(args):
    bios, tcg = "--bios" in args, "--tcg" in args
    if bios and tcg:
        target = "run"            # the plain BIOS + TCG boot
    elif bios:
        target = "run-kvm"
    elif tcg:
        target = "run-uefi"
    else:
        target = "run-uefi-kvm"   # the real thing: UEFI + KVM
    print("prism: make %s (the image first, then QEMU)" % target)
    sys.exit(subprocess.call(["make", "-C", REPO, target]))


def cmd_clean(args):
    if not args or args == ["--all"]:
        shutil.rmtree(os.path.join(BUILD_DIR, "apps-out"), ignore_errors=True)
        for app in find_apps():
            shutil.rmtree(app.app_rel("build"), ignore_errors=True)
        print("prism: cleaned every app's outputs")
        return
    for name in args:
        app = get_app(name)
        shutil.rmtree(app.app_rel("build"), ignore_errors=True)
        if os.path.exists(app.staged):
            os.remove(app.staged)
        print("prism: cleaned %s" % app.name)


USAGE = """\
usage: python3 tools/prism.py <command> [args]

  init <name>      scaffold a new package under apps/<name>
  build [name]     build one package, or all of them
  install [name]   install built binaries into a root tree (--dest DIR)
  verify [name]    run verify.sh's contract checks alone
  list             list the packages under apps/
  run              make the image and boot it (--bios, --tcg select the mode)
  clean [name]     remove a package's outputs, or all of them (--all)
"""


def main(argv):
    if not argv or argv[0] in ("-h", "--help", "help"):
        print(USAGE, end="")
        return 0
    cmd, args = argv[0], argv[1:]
    dispatch = {"init": cmd_init, "build": cmd_build, "install": cmd_install,
                "verify": cmd_verify, "list": cmd_list, "run": cmd_run,
                "clean": cmd_clean}
    if cmd not in dispatch:
        print(USAGE, end="", file=sys.stderr)
        return die("unknown command %r" % cmd)
    try:
        dispatch[cmd](args)
    except subprocess.CalledProcessError as e:
        die("command failed with exit status %d" % e.returncode)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
