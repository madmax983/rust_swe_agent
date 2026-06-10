use std::collections::{HashSet, BTreeSet};

fn get_unknown() {
    let ids: Vec<String> = vec!["a".to_owned(), "b".to_owned()];
    let dataset_ids: HashSet<&str> = HashSet::new();
    let unknown: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| !dataset_ids.contains(id))
        .collect();
    if !unknown.is_empty() {
        println!("{}", unknown.join(", "));
    }
}
fn main() { get_unknown(); }
