/// Sum of the integers from 1 to `n` inclusive.
pub fn sum_to(n: u32) -> u32 {
    (1..n).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_small_ranges() {
        assert_eq!(sum_to(1), 1);
        assert_eq!(sum_to(4), 10);
    }

    #[test]
    fn zero_is_empty() {
        assert_eq!(sum_to(0), 0);
    }
}
