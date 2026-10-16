//! Candidate index for immutable mount fallback. Exact lookup stays live.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub(super) struct FallbackIndex {
    by_name: HashMap<String, Vec<PathBuf>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_candidates_preserve_suffix_ranking_and_ambiguity() {
        let root = Path::new("card");
        let files: Vec<PathBuf> = [
            "card/Wrapper/Assets/ITEM.BIN",
            "card/Other/item.bin",
            "card/Other/item.bin.extra",
            "card/Other/ONLY.BIN",
            "card/Third/item.bin",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        let index = FallbackIndex::new(files.clone());
        for wanted in [
            vec!["sd card", "assets", "item.bin"],
            vec!["sd card", "unknown", "ITEM.BIN"],
            vec!["sd card", "unknown", "only.bin"],
            vec!["missing.bin"],
            vec!["item.bin.extra"],
            vec![],
        ] {
            assert_eq!(
                rank(root, &wanted, index.candidates(&wanted)),
                rank(root, &wanted, &files),
                "{wanted:?}"
            );
        }
        assert_eq!(index.candidates(&["missing.bin"]).len(), 0);
        assert_eq!(index.candidates(&["ONLY.BIN"]).len(), 1);
        assert_eq!(index.candidates(&["item.bin"]).len(), 3);
    }

    #[test]
    fn ascii_case_matching_retains_non_ascii_distinctions() {
        let files: Vec<PathBuf> = ["card/é.BIN", "card/É.BIN"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        let index = FallbackIndex::new(files.clone());
        for name in ["é.bin", "É.bin", "e.bin"] {
            let wanted = [name];
            assert_eq!(
                rank(Path::new("card"), &wanted, index.candidates(&wanted)),
                rank(Path::new("card"), &wanted, &files)
            );
        }
    }

    #[test]
    fn repeated_misses_do_not_visit_unrelated_files() {
        let files: Vec<PathBuf> = (0..50_000)
            .map(|n| PathBuf::from(format!("card/assets/asset{n}.bin")))
            .collect();
        let index = FallbackIndex::new(files.clone());
        let wanted = ["absent.bwav"];
        let begin = std::time::Instant::now();
        for _ in 0..64 {
            assert_eq!(rank(Path::new("card"), &wanted, &files), (0, vec![]));
        }
        let old = begin.elapsed();
        let begin = std::time::Instant::now();
        for _ in 0..64 {
            let candidates = index.candidates(&wanted);
            assert!(candidates.is_empty());
            assert_eq!(rank(Path::new("card"), &wanted, candidates), (0, vec![]));
        }
        println!(
            "64 fallback misses over 50,000 files: full scan {old:?}, indexed {:?}",
            begin.elapsed()
        );
    }
}

impl FallbackIndex {
    pub(super) fn new(files: Vec<PathBuf>) -> Self {
        let mut index = Self::default();
        for path in files {
            if let Some(name) = path.file_name() {
                let key = name.to_string_lossy().to_ascii_lowercase();
                index.by_name.entry(key).or_default().push(path);
            }
        }
        index
    }

    pub(super) fn candidates(&self, wanted: &[&str]) -> &[PathBuf] {
        wanted
            .last()
            .and_then(|name| self.by_name.get(&name.to_ascii_lowercase()))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

pub(super) fn rank(root: &Path, wanted: &[&str], files: &[PathBuf]) -> (usize, Vec<PathBuf>) {
    let mut best = 0;
    let mut found = Vec::new();
    for path in files {
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let score = relative
            .components()
            .rev()
            .zip(wanted.iter().rev())
            .take_while(|(component, expected)| {
                component
                    .as_os_str()
                    .to_string_lossy()
                    .eq_ignore_ascii_case(expected)
            })
            .count();
        if score > best {
            best = score;
            found.clear();
        }
        if score != 0 && score == best && found.len() < 2 {
            found.push(path.clone());
        }
    }
    (best, found)
}
