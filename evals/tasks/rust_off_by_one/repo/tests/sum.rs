use summer::sum_to;

#[test]
fn sums_small_ranges() {
    assert_eq!(sum_to(1), 1);
    assert_eq!(sum_to(4), 10);
    assert_eq!(sum_to(100), 5050);
}

#[test]
fn zero_is_empty() {
    assert_eq!(sum_to(0), 0);
}
