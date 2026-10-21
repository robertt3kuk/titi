import unittest

from textutil import shout, slugify


class TextUtilTest(unittest.TestCase):
    def test_shout(self):
        self.assertEqual(shout("hi"), "HI!")

    def test_slugify_basic(self):
        self.assertEqual(slugify("Hello, World"), "hello-world")

    def test_slugify_runs_and_edges(self):
        self.assertEqual(slugify("  --A  b__c--  "), "a-b-c")

    def test_slugify_empty(self):
        self.assertEqual(slugify("!!!"), "")


if __name__ == "__main__":
    unittest.main()
