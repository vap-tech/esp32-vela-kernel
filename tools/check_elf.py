"""Check linked symbols/sections independently of the linker script."""
import re
import shutil
import subprocess
import sys
readelf = shutil.which("llvm-readelf") or "readelf"
out = subprocess.check_output([readelf, "--wide", "-S", "-s", sys.argv[1]], text=True)
symbols = {}
for line in out.splitlines():
    fields = line.split()
    if len(fields) == 8 and fields[0].endswith(":"):
        try: symbols[fields[7]] = int(fields[1], 16)
        except ValueError: pass
s = symbols
assert s['_start'] == s['_entry_start'] == 0x30100000
assert 0 < s['_entry_end'] - s['_entry_start'] <= 8192
assert s['_upper_start'] == 0x4ff40000
assert s['_upper_start'] < s['_upper_load_end'] <= s['_bss_start'] <= s['_bss_end'] <= s['_stack_bottom']
assert s['_stack_bottom'] == 0x4ffac000 and s['_stack_top'] == 0x4ffc0000
assert s['_boot_stack_top'] == 0x4ff28000
assert s['vela_trap_entry'] % 64 == 0
for name in ['_bss_start', '_bss_end', '_stack_top', '_boot_stack_top']:
    assert s[name] % 16 == 0
assert not any(len(f := line.split()) == 8 and f[6] == 'UND' for line in out.splitlines())
print(f"ELF verified: entry={s['_entry_end']-s['_entry_start']} B; payload={s['_upper_load_end']-s['_upper_start']} B; BSS={s['_bss_end']-s['_bss_start']} B")
