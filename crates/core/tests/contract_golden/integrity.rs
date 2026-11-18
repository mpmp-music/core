//! Pinned file inventory and SHA-256 for every frozen set. Never refresh old sets.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::frozen_manifest::FROZEN_FILES;

fn inventory(dir: &Path, prefix: &str, out: &mut BTreeMap<String, String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = format!("{prefix}/{}", entry.file_name().to_str().unwrap());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            inventory(&entry.path(), &name, out);
        } else {
            assert!(
                kind.is_file(),
                "frozen entry must be a regular file: {name}"
            );
            let bytes = std::fs::read(entry.path()).unwrap();
            out.insert(name, format!("{:x}", Sha256::digest(bytes)));
        }
    }
}

fn expected() -> BTreeMap<String, String> {
    let expected: BTreeMap<_, _> = FROZEN_FILES
        .iter()
        .map(|(path, hash)| ((*path).to_owned(), (*hash).to_owned()))
        .collect();
    assert_eq!(expected.len(), FROZEN_FILES.len(), "duplicate pinned paths");
    expected
}

fn frozen_sets(root: &Path) -> BTreeSet<String> {
    std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter_map(|entry| {
            let name = entry.file_name().into_string().unwrap();
            if !name.starts_with('v') {
                return None;
            }
            assert!(
                entry.file_type().unwrap().is_dir(),
                "invalid frozen set: {name}"
            );
            Some(name)
        })
        .collect()
}

fn changes(expected: &BTreeMap<String, String>, actual: &BTreeMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    for (path, hash) in expected {
        match actual.get(path) {
            None => out.push(format!("{path}: missing")),
            Some(value) if value != hash => out.push(format!("{path}: SHA-256 changed")),
            _ => {}
        }
    }
    for path in actual.keys() {
        if !expected.contains_key(path) {
            out.push(format!("{path}: unexpected file"));
        }
    }
    out
}

#[test]
fn frozen_file_inventory_and_sha256_are_pinned() {
    let expected = expected();
    let sets: std::collections::BTreeSet<_> = expected
        .keys()
        .map(|p| p.split('/').next().unwrap())
        .collect();
    assert_eq!(sets, super::FROZEN_SETS.iter().copied().collect());
    assert_eq!(
        frozen_sets(&super::golden_dir()),
        sets.iter().map(|set| (*set).to_owned()).collect(),
        "undeclared or missing frozen set"
    );
    let mut actual = BTreeMap::new();
    for set in super::FROZEN_SETS {
        inventory(&super::golden_dir().join(set), set, &mut actual);
    }
    assert_eq!(changes(&expected, &actual), Vec::<String>::new());
}

#[test]
fn integrity_guard_detects_undeclared_and_missing_frozen_directories() {
    let root = tempfile::tempdir().unwrap();
    let expected: BTreeSet<_> = super::FROZEN_SETS.iter().map(|s| (*s).to_owned()).collect();
    std::fs::create_dir(root.path().join("current")).unwrap();
    for set in super::FROZEN_SETS {
        std::fs::create_dir(root.path().join(set)).unwrap();
    }
    assert_eq!(frozen_sets(root.path()), expected);
    std::fs::create_dir(root.path().join("v1.1")).unwrap();
    assert_ne!(frozen_sets(root.path()), expected);
    std::fs::remove_dir(root.path().join("v1.1")).unwrap();
    std::fs::remove_dir(root.path().join(super::FROZEN_SETS[0])).unwrap();
    assert_ne!(frozen_sets(root.path()), expected);
}

#[test]
fn integrity_guard_detects_edits_deletions_additions_and_renames_in_every_set() {
    let expected = expected();
    for set in super::FROZEN_SETS {
        let path = format!("{set}/contract.schema.json");
        for mutation in ["edit", "delete", "add", "rename"] {
            let mut actual = expected.clone();
            match mutation {
                "edit" => {
                    actual.insert(path.clone(), "0".repeat(64));
                }
                "delete" => {
                    actual.remove(&path);
                }
                "add" => {
                    actual.insert(format!("{set}/extra.json"), "0".repeat(64));
                }
                "rename" => {
                    let hash = actual.remove(&path).unwrap();
                    actual.insert(format!("{set}/renamed.json"), hash);
                }
                _ => unreachable!(),
            }
            assert!(!changes(&expected, &actual).is_empty(), "{set}: {mutation}");
        }
    }
}
