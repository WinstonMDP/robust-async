pub fn is_sorted<T: Clone + Ord>(a: &[T]) -> bool {
    let mut c = a.to_vec();
    c.sort_unstable();
    a == c
}
