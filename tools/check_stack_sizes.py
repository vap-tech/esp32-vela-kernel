"""Check static frame sizes in the final LTO RISC-V assembly's CFI metadata.

CFI describes stack frame allocation even when registers reserve unwritten space.
This does not calculate nested call depth or cover handwritten assembly.
"""

import argparse
import re

LIMIT = 24 * 1024


def read_sizes(source):
    frames = []
    label = None
    current = None
    for line in source.splitlines():
        match = re.match(r"^([^\s.][^\s:]*):", line)
        if match:
            label = match[1]
        if line.strip() == ".cfi_startproc":
            if label is None or current is not None:
                raise ValueError("invalid CFI function boundary")
            current = (0, label)
        match = re.match(r"\s*\.cfi_def_cfa_offset\s+(\d+)\s*$", line)
        if match and current is not None:
            current = (max(current[0], int(match[1])), current[1])
        if line.strip() == ".cfi_endproc":
            if current is None:
                raise ValueError("CFI end without start")
            frames.append(current)
            current = None
    if not frames or current is not None or not any(n == "rust_main" for _, n in frames):
        raise ValueError("missing or incomplete final kernel CFI metadata")
    return sorted(frames, reverse=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("assembly")
    parser.add_argument("--limit", type=int, default=LIMIT)
    args = parser.parse_args()
    with open(args.assembly) as file:
        sizes = read_sizes(file.read())
    for size, name in sizes[:5]:
        print(f"Stack frame: {size:6d} B {name}")
    if sizes[0][0] > args.limit:
        raise SystemExit(f"Kernel frame exceeds {args.limit} B budget (nested calls excluded)")
    print(f"Kernel frames verified: {len(sizes)} entries, limit={args.limit} B")


if __name__ == "__main__":
    main()
