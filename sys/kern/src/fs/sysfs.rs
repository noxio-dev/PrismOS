// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (c) 2026 noxio-dev
//
// sysfs: the device tree libdrm, udev-less X servers and `evtest`-style tools
// read to discover hardware.  Read-only, built once at mount time from what
// the kernel found (is there a DRM device, is there a PS/2 mouse), with a few
// files generated on every read (the panel's mode, the active VT).
//
//   /sys/dev/char/226:0                    -> the DRM card (libdrm's first stop)
//   /sys/dev/char/13:64.. 13:65            -> the evdev nodes
//   /sys/class/drm/card0, card0-Virtual-1  -> the DRM card and its connector
//   /sys/class/input/inputN, eventN        -> the input devices
//   /sys/class/tty/tty0/active             -> "ttyN", the active VT
//   /sys/devices/platform/prismfb/...      -> the display, a platform device
//   /sys/devices/platform/i8042/serio{0,1}/input/inputN/...
//   /sys/bus/platform                      -> the bus the display sits on
//
// libdrm decides what bus a card is on by reading the basename of
// /sys/dev/char/226:0/device/subsystem ("platform" here) and then parses
// .../device/uevent; both are provided.  Symlink targets are absolute
// (/sys/...), which the VFS walk resolves without ".." gymnastics.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::fs::vfs::{self, DirEntry, FileSystem, FileType, FsError, FsResult, Stat, StatFs};

enum Body {
    Dir,
    Text(String),
    Gen(fn() -> String),
    Link(String),
}

struct Node {
    name: String,
    parent: usize,
    body: Body,
    children: Vec<usize>,
}

pub struct SysFs {
    nodes: Vec<Node>,
}

impl SysFs {
    pub fn new() -> SysFs {
        let mut fs = SysFs { nodes: Vec::new() };
        fs.nodes.push(Node { name: String::new(), parent: 0, body: Body::Dir, children: Vec::new() });
        fs.populate();
        fs
    }

    fn add(&mut self, parent: usize, name: &str, body: Body) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(Node { name: String::from(name), parent, body, children: Vec::new() });
        self.nodes[parent].children.push(idx);
        idx
    }

    /// `a/b/c` as directories, creating what is missing.
    fn path(&mut self, p: &str) -> usize {
        let mut cur = 0;
        for part in p.split('/').filter(|s| !s.is_empty()) {
            let found = self.nodes[cur].children.iter().copied().find(|&c| self.nodes[c].name == part);
            cur = match found {
                Some(c) => c,
                None => self.add(cur, part, Body::Dir),
            };
        }
        cur
    }

    fn text(&mut self, dir: usize, name: &str, s: &str) {
        self.add(dir, name, Body::Text(String::from(s)));
    }

    fn link(&mut self, dir: usize, name: &str, target: &str) {
        self.add(dir, name, Body::Link(String::from(target)));
    }

    fn populate(&mut self) {
        self.path("bus/platform");
        self.path("class/graphics");
        let class_drm = self.path("class/drm");
        let class_input = self.path("class/input");
        let dev_char = self.path("dev/char");

        // --- the active VT ------------------------------------------------
        let tty0 = self.path("class/tty/tty0");
        self.add(tty0, "active", Body::Gen(|| format!("tty{}\n", crate::user::vt::active())));

        // --- the display ---------------------------------------------------
        if crate::drm::present() {
            let plat = self.path("devices/platform/prismfb");
            self.text(plat, "uevent", "DRIVER=prismfb\nMODALIAS=platform:prismfb\n");
            self.text(plat, "modalias", "platform:prismfb\n");
            self.link(plat, "subsystem", "/sys/bus/platform");
            let card = self.path("devices/platform/prismfb/drm/card0");
            self.text(card, "dev", "226:0\n");
            self.text(card, "uevent", "MAJOR=226\nMINOR=0\nDEVNAME=dri/card0\nDEVTYPE=drm_minor\n");
            self.link(card, "device", "/sys/devices/platform/prismfb");
            self.link(card, "subsystem", "/sys/class/drm");
            let conn = self.path("devices/platform/prismfb/drm/card0/card0-Virtual-1");
            self.text(conn, "status", "connected\n");
            self.text(conn, "enabled", "enabled\n");
            self.text(conn, "dpms", "On\n");
            self.text(conn, "uevent", "");
            self.add(
                conn,
                "modes",
                Body::Gen(|| match crate::drm::panel_size() {
                    Some((w, h)) => format!("{}x{}\n", w, h),
                    None => String::new(),
                }),
            );
            self.link(conn, "device", "/sys/devices/platform/prismfb/drm/card0");
            self.link(class_drm, "card0", "/sys/devices/platform/prismfb/drm/card0");
            self.link(class_drm, "card0-Virtual-1", "/sys/devices/platform/prismfb/drm/card0/card0-Virtual-1");
            self.text(class_drm, "version", "drm 1.1.0 20060810\n");
            self.link(dev_char, "226:0", "/sys/devices/platform/prismfb/drm/card0");
        }

        // --- input devices -------------------------------------------------
        let mut n = 0usize;
        for dev in 0..crate::input::N_DEV {
            if dev == crate::input::DEV_MOUSE && !crate::hw::kbd::mouse_present() {
                continue;
            }
            let serio = format!("devices/platform/i8042/serio{}", dev);
            let inp_path = format!("{}/input/input{}", serio, n);
            let inp = self.path(&inp_path);
            let name = crate::input::dev_name(dev);
            let phys = crate::input::dev_phys(dev);
            let (bus, ven, prod, ver) = crate::input::dev_id(dev);
            self.text(inp, "name", &format!("{}\n", name));
            self.text(inp, "phys", &format!("{}\n", phys));
            self.text(inp, "uniq", "\n");
            self.text(inp, "properties", "0\n");
            self.text(
                inp,
                "uevent",
                &format!(
                    "PRODUCT={:x}/{:x}/{:x}/{:x}\nNAME=\"{}\"\nPHYS=\"{}\"\nPROP=0\nEV={}\nKEY={}\n",
                    bus,
                    ven,
                    prod,
                    ver,
                    name,
                    phys,
                    crate::input::caps_hex(dev, None),
                    crate::input::caps_hex(dev, Some(crate::input::EV_KEY)),
                ),
            );
            let id = self.path(&format!("{}/id", inp_path));
            self.text(id, "bustype", &format!("{:04x}\n", bus));
            self.text(id, "vendor", &format!("{:04x}\n", ven));
            self.text(id, "product", &format!("{:04x}\n", prod));
            self.text(id, "version", &format!("{:04x}\n", ver));
            let caps = self.path(&format!("{}/capabilities", inp_path));
            for (file, ev) in [
                ("ev", None),
                ("key", Some(crate::input::EV_KEY)),
                ("rel", Some(crate::input::EV_REL)),
                ("abs", Some(crate::input::EV_ABS)),
                ("msc", Some(crate::input::EV_MSC)),
                ("led", Some(crate::input::EV_LED)),
                ("sw", Some(crate::input::EV_SW)),
                ("ff", Some(crate::input::EV_FF)),
                ("snd", Some(crate::input::EV_SND)),
            ] {
                self.text(caps, file, &format!("{}\n", crate::input::caps_hex(dev, ev)));
            }
            let ev_path = format!("{}/event{}", inp_path, dev);
            let evd = self.path(&ev_path);
            let minor = 64 + dev;
            self.text(evd, "dev", &format!("13:{}\n", minor));
            self.text(
                evd,
                "uevent",
                &format!("MAJOR=13\nMINOR={}\nDEVNAME=input/event{}\n", minor, dev),
            );
            self.link(evd, "device", &format!("/sys/{}", inp_path));
            self.link(evd, "subsystem", "/sys/class/input");
            self.link(inp, "subsystem", "/sys/class/input");
            self.link(class_input, &format!("input{}", n), &format!("/sys/{}", inp_path));
            self.link(class_input, &format!("event{}", dev), &format!("/sys/{}", ev_path));
            self.link(dev_char, &format!("13:{}", minor), &format!("/sys/{}", ev_path));
            n += 1;
        }
    }

    fn node(&self, ino: u64) -> FsResult<&Node> {
        ino.checked_sub(1)
            .and_then(|i| self.nodes.get(i as usize))
            .ok_or(FsError::NotFound)
    }

    fn content(&self, n: &Node) -> String {
        match &n.body {
            Body::Text(s) => s.clone(),
            Body::Gen(f) => f(),
            _ => String::new(),
        }
    }

    fn stat_node(&self, idx: usize) -> Stat {
        let n = &self.nodes[idx];
        let (ftype, mode, size) = match &n.body {
            Body::Dir => (FileType::Dir, 0o555, 0),
            Body::Link(t) => (FileType::Symlink, 0o777, t.len() as u64),
            _ => (FileType::Regular, 0o444, self.content(n).len() as u64),
        };
        let now = vfs::now();
        Stat {
            ino: idx as u64 + 1,
            ftype,
            mode,
            size,
            nlink: 1,
            uid: 0,
            gid: 0,
            atime: now,
            mtime: now,
            ctime: now,
            blocks: size.div_ceil(512),
        }
    }
}

fn ftype_of(b: &Body) -> FileType {
    match b {
        Body::Dir => FileType::Dir,
        Body::Link(_) => FileType::Symlink,
        _ => FileType::Regular,
    }
}

impl FileSystem for SysFs {
    fn fs_name(&self) -> &'static str {
        "sysfs"
    }

    fn root(&self) -> u64 {
        1
    }

    fn read_only(&self) -> bool {
        true
    }

    fn statfs(&mut self) -> FsResult<StatFs> {
        Ok(StatFs {
            block_size: 4096,
            blocks: 0,
            free_blocks: 0,
            files: self.nodes.len() as u64,
            free_files: 0,
            name_max: 255,
        })
    }

    fn stat(&mut self, ino: u64) -> FsResult<Stat> {
        self.node(ino)?;
        Ok(self.stat_node(ino as usize - 1))
    }

    fn lookup(&mut self, dir: u64, name: &str) -> FsResult<u64> {
        let d = self.node(dir)?;
        if !matches!(d.body, Body::Dir) {
            return Err(FsError::NotDir);
        }
        if name == "." {
            return Ok(dir);
        }
        if name == ".." {
            return Ok(d.parent as u64 + 1);
        }
        d.children
            .iter()
            .copied()
            .find(|&c| self.nodes[c].name == name)
            .map(|c| c as u64 + 1)
            .ok_or(FsError::NotFound)
    }

    fn readdir(&mut self, dir: u64) -> FsResult<Vec<DirEntry>> {
        let d = self.node(dir)?;
        if !matches!(d.body, Body::Dir) {
            return Err(FsError::NotDir);
        }
        let mut out = Vec::with_capacity(d.children.len() + 2);
        out.push(DirEntry { ino: dir, ftype: FileType::Dir, name: String::from(".") });
        out.push(DirEntry { ino: d.parent as u64 + 1, ftype: FileType::Dir, name: String::from("..") });
        for &c in &d.children {
            let n = &self.nodes[c];
            out.push(DirEntry { ino: c as u64 + 1, ftype: ftype_of(&n.body), name: n.name.to_string() });
        }
        Ok(out)
    }

    fn read(&mut self, ino: u64, off: u64, buf: &mut [u8]) -> FsResult<usize> {
        let n = self.node(ino)?;
        match n.body {
            Body::Dir => return Err(FsError::IsDir),
            Body::Link(_) => return Err(FsError::Invalid),
            _ => {}
        }
        let c = self.content(n);
        let b = c.as_bytes();
        let off = off as usize;
        if off >= b.len() {
            return Ok(0);
        }
        let k = (b.len() - off).min(buf.len());
        buf[..k].copy_from_slice(&b[off..off + k]);
        Ok(k)
    }

    fn readlink(&mut self, ino: u64) -> FsResult<String> {
        match &self.node(ino)?.body {
            Body::Link(t) => Ok(t.clone()),
            _ => Err(FsError::Invalid),
        }
    }
}
