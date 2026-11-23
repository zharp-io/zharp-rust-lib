pub fn total(items: &[Option<u32>]) -> u32 {
    items.iter().map(|i| i.unwrap()).sum()
}
