from pricing import calc_total


def average_order(orders):
    totals = [calc_total(order) for order in orders]
    return sum(totals) / len(totals)
