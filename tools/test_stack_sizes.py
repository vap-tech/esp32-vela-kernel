import unittest

from check_stack_sizes import LIMIT, read_sizes


class StackSizesTest(unittest.TestCase):
    def test_large_split_prologue_is_not_hidden_by_epilogue(self):
        frames = read_sizes("""rust_main:
 .cfi_startproc
 .cfi_def_cfa_offset 256
 .cfi_def_cfa_offset 43760
 .cfi_def_cfa_offset 256
 .cfi_def_cfa_offset 0
 .cfi_endproc
""")
        self.assertEqual(frames, [(43760, "rust_main")])
        self.assertGreater(frames[0][0], LIMIT)

    def test_all_functions_count(self):
        frames = read_sizes("""rust_main:
 .cfi_startproc
 .cfi_def_cfa_offset 17088
 .cfi_endproc
helper:
 .cfi_startproc
 .cfi_def_cfa_offset 30000
 .cfi_endproc
""")
        self.assertEqual(frames[0], (30000, "helper"))

    def test_missing_metadata_fails(self):
        with self.assertRaises(ValueError):
            read_sizes("rust_main:\n ret\n")

    def test_truncated_metadata_fails(self):
        with self.assertRaises(ValueError):
            read_sizes("rust_main:\n .cfi_startproc\n .cfi_def_cfa_offset 16\n")
