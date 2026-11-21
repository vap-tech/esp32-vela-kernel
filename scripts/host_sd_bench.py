#!/usr/bin/env python3
"""Host-side raw microSD benchmark, same layout as `sdcard bench` on the Tab5.

Writes random data at LBA 16384 (8 MiB), reads it back with O_DIRECT and verifies.
Opens the block device via UDisks2 (no root needed for removable media).

usage: scripts/host_sd_bench.py [DEV=sda] [MIB=100] [CHUNK_KIB=32]
"""
import mmap
import os
import sys
import time

from gi.repository import Gio, GLib

dev = sys.argv[1] if len(sys.argv) > 1 else "sda"
mib = int(sys.argv[2]) if len(sys.argv) > 2 else 100
chunk = (int(sys.argv[3]) if len(sys.argv) > 3 else 32) * 1024
OFFSET = 16384 * 512

bus = Gio.bus_get_sync(Gio.BusType.SYSTEM)
proxy = Gio.DBusProxy.new_sync(
    bus, 0, None, "org.freedesktop.UDisks2",
    f"/org/freedesktop/UDisks2/block_devices/{dev}",
    "org.freedesktop.UDisks2.Block", None)
size = proxy.get_cached_property("Size").get_uint64()
if size > 64 << 30:
    sys.exit("refusing: device larger than 64 GiB")


def open_dev(flags):
    res, fds = proxy.call_with_unix_fd_list_sync(
        "OpenDevice", GLib.Variant("(sa{sv})", ("rw", {"flags": GLib.Variant("i", flags)})),
        0, -1, None, None)
    return fds.get(res.unpack()[0])


fd = open_dev(os.O_DIRECT | os.O_SYNC)
total = mib << 20
buf = mmap.mmap(-1, chunk)
data = [os.urandom(chunk) for _ in range(8)]  # 8 distinct random chunks, cycled

t0 = time.perf_counter()
for i, off in enumerate(range(0, total, chunk)):
    buf.seek(0)
    buf.write(data[i % 8])
    os.pwritev(fd, [buf], OFFSET + off)
os.fsync(fd)
tw = time.perf_counter() - t0

t0 = time.perf_counter()
bad = 0
for i, off in enumerate(range(0, total, chunk)):
    os.preadv(fd, [buf], OFFSET + off)
    if buf[:] != data[i % 8]:
        bad += 1
tr = time.perf_counter() - t0
os.close(fd)
print(f"[host-bench] dev={dev} size={mib}MiB chunk={chunk // 1024}KiB "
      f"write={tw * 1000:.0f}ms ({mib / tw:.2f}MiB/s) "
      f"read={tr * 1000:.0f}ms ({mib / tr:.2f}MiB/s) verify={'ok' if not bad else f'BAD({bad})'}")
