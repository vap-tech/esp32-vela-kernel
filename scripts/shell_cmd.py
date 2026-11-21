#!/usr/bin/env python3
"""Send shell commands to VELA on /dev/ttyACM0 and print the live output.

usage: scripts/shell_cmd.py [-t SECONDS] [-w BOOT_WAIT] [--reset] CMD [CMD ...]
Each CMD waits for the next `vela:/$ ` prompt (or SECONDS timeout).
--reset pulses RTS (EN) with DTR low (GPIO0 high => normal boot) and logs the boot.
"""

import argparse
import fcntl
import os
import select
import struct
import sys
import termios
import time

PORT = "/dev/ttyACM0"
PROMPT = b"vela:/$ "
TIOCMBIS = 0x5416
TIOCMBIC = 0x5417
TIOCM_DTR = 0x002
TIOCM_RTS = 0x004


def modem(fd, req, bits):
    fcntl.ioctl(fd, req, struct.pack("I", bits))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-t", type=float, default=8.0, help="timeout per command")
    ap.add_argument("-w", type=float, default=0.0, help="initial wait (boot)")
    ap.add_argument("--reset", action="store_true")
    ap.add_argument("cmds", nargs="*")
    a = ap.parse_args()

    fd = os.open(PORT, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    attr = termios.tcgetattr(fd)
    attr[0] = 0
    attr[1] = 0
    attr[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
    attr[3] = 0
    attr[4] = attr[5] = termios.B115200
    termios.tcsetattr(fd, termios.TCSANOW, attr)

    def read_until(timeout, stop=None):
        buf = b""
        end = time.time() + timeout
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    d = os.read(fd, 4096)
                except BlockingIOError:
                    continue
                buf += d
                sys.stdout.write(d.decode(errors="replace").replace("\r\r", "\r"))
                sys.stdout.flush()
                if stop and buf.endswith(stop):
                    break
        return buf

    if a.reset:
        modem(fd, TIOCMBIC, TIOCM_DTR)  # GPIO0 high
        modem(fd, TIOCMBIS, TIOCM_RTS)  # EN low
        time.sleep(0.1)
        modem(fd, TIOCMBIC, TIOCM_RTS)  # EN high
        print("[host] reset pulsed, boot log:")
    if a.w:
        read_until(a.w)
    os.write(fd, b"\n")
    read_until(2.0, PROMPT)
    for c in a.cmds:
        print(f"\n[host] > {c}")
        os.write(fd, c.encode() + b"\n")
        read_until(a.t, PROMPT)
    print("\n[host] done")
    sys.stdout.flush()
    try:
        termios.tcflush(fd, termios.TCOFLUSH)
    except OSError:
        pass
    os._exit(0)


if __name__ == "__main__":
    main()
