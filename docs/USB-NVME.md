# NVMe and USB (xHCI)

Drivers: `sys/kern/src/hw/{nvme,xhci,usbhid,usbmsc,disk,hal}.rs`.

* **NVMe** -- polling, one admin + one I/O queue, 4 KiB-page controllers,
  read / write / flush, transfers up to MDTS (<= 128 KiB per command).
* **xHCI** -- BIOS handoff, reset, command/event/transfer rings, root-port
  enumeration (no hubs yet), HID boot keyboard (interrupt IN, auto-repeat),
  mass storage (Bulk-Only / SCSI, LUN 0). Hot plug is manual: `usb rescan`.
* **Disk registry** (`disk.rs`): SATA, then NVMe, then USB, all as 512-byte
  sector `BlockDev`s; 4 KiB-block devices are adapted (read-modify-write).
  `fsys` mounts "/" from the first ext volume of any of them.
* Command line: `nvme=off`, `xhci=off`. Shell: `disks`, `usb`, `usb rescan`.

QEMU:

    -drive file=disk.img,if=none,id=nv,format=raw -device nvme,drive=nv,serial=prism0
    -device qemu-xhci -device usb-kbd
    -drive file=stick.img,if=none,id=us,format=raw -device usb-storage,drive=us

Host tests (software models of the controllers, real driver sources):

    cd tests/hw_host && for s in nvme512 nvme4k usb32 usb64; do cargo run -q -- $s; done

Not covered: USB hubs, multi-LUN, UAS, isochronous, interrupts (MSI-X),
multiple NVMe I/O queues, NVMe namespaces with metadata.
