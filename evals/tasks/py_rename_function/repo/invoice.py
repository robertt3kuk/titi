from pricing import calc_total


def render(prices, tax_rate=0.0):
    return "TOTAL: %.2f" % calc_total(prices, tax_rate)
