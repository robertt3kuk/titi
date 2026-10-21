import unittest

from invoice import render
from pricing import compute_total
from report import average_order


class PricingTest(unittest.TestCase):
    def test_total(self):
        self.assertEqual(compute_total([10, 20], 0.1), 33.0)

    def test_invoice(self):
        self.assertEqual(render([5, 5]), "TOTAL: 10.00")

    def test_average(self):
        self.assertEqual(average_order([[10], [20, 10]]), 20.0)


if __name__ == "__main__":
    unittest.main()
