import contextlib
import io
import os
import tempfile
import unittest

import wc


def run(argv):
    out = io.StringIO()
    with contextlib.redirect_stdout(out):
        code = wc.main(argv)
    return code, out.getvalue()


class WcTest(unittest.TestCase):
    def setUp(self):
        handle = tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False)
        handle.write("one two three\nfour\n")
        handle.close()
        self.path = handle.name

    def tearDown(self):
        os.unlink(self.path)

    def test_plain(self):
        self.assertEqual(run([self.path]), (0, "4\n"))

    def test_verbose(self):
        self.assertEqual(run(["--verbose", self.path]), (0, "reading %s\n4\n" % self.path))


if __name__ == "__main__":
    unittest.main()
