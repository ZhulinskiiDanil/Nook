//! Duplicate detection: blake3, only for files that share a size with another file.

use std::collections::HashMap;
use std::path::PathBuf;

/// `(key, path, size)` in → groups of keys whose content is identical (each group ≥ 2).
pub fn find_duplicates<K: Clone + Eq + std::hash::Hash>(
    files: &[(K, PathBuf, u64)],
) -> Vec<Vec<K>> {
    let mut by_size: HashMap<u64, Vec<&(K, PathBuf, u64)>> = HashMap::new();
    for f in files.iter().filter(|f| f.2 > 0) {
        by_size.entry(f.2).or_default().push(f);
    }
    let mut groups = Vec::new();
    for same_size in by_size.into_values().filter(|v| v.len() > 1) {
        let mut by_hash: HashMap<[u8; 32], Vec<K>> = HashMap::new();
        for (k, path, _) in same_size {
            if let Some(h) = hash_file(path) {
                by_hash.entry(h).or_default().push(k.clone());
            }
        }
        groups.extend(by_hash.into_values().filter(|v| v.len() > 1));
    }
    groups
}

pub fn hash_file(path: &std::path::Path) -> Option<[u8; 32]> {
    let mut h = blake3::Hasher::new();
    h.update_reader(std::fs::File::open(path).ok()?).ok()?;
    Some(*h.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_same_content_only() {
        let d = tempfile::tempdir().unwrap();
        let w = |n: &str, c: &[u8]| {
            let p = d.path().join(n);
            std::fs::write(&p, c).unwrap();
            (n.to_string(), p, c.len() as u64)
        };
        let files = vec![
            w("robot_03.png", b"PIXELS-A"),
            w("robot_03 копия 2.png", b"PIXELS-A"),
            w("robot_04.png", b"PIXELS-B"), // same size, different content
            w("empty1.txt", b""),
            w("empty2.txt", b""),
        ];
        let mut groups = find_duplicates(&files);
        assert_eq!(groups.len(), 1);
        groups[0].sort();
        assert_eq!(
            groups[0],
            vec![
                "robot_03 копия 2.png".to_string(),
                "robot_03.png".to_string()
            ]
        );
    }
}
