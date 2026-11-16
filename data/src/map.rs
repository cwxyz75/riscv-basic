//! The ordered map: insertion order, string keys, a duplicate key is an error (SHELL-SPEC section 4, rule 6).

use crate::Value;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DuplicateKey;

#[derive(Debug, Clone, Default)]
pub struct Map {
    entries: Vec<(String, Value)>,
    index: BTreeMap<String, usize>,
}

impl Map {
    pub fn new() -> Map {
        Map::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Add a key that is not there yet.
    pub fn insert(&mut self, key: String, value: Value) -> Result<(), DuplicateKey> {
        if self.index.contains_key(&key) {
            return Err(DuplicateKey);
        }
        self.index.insert(key.clone(), self.entries.len());
        self.entries.push((key, value));
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.index.get(key).map(|&i| &self.entries[i].1)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    /// In insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    /// A copy with `key` set: replaced in place if present (keeping its position), else appended.
    pub fn with(&self, key: &str, value: Value) -> Map {
        let mut m = self.clone();
        match m.index.get(key) {
            Some(&i) => m.entries[i].1 = value,
            None => {
                let _ = m.insert(String::from(key), value);
            }
        }
        m
    }

    /// A copy without `key` (the same map if it was not there).
    pub fn without(&self, key: &str) -> Map {
        let mut m = Map::new();
        for (k, v) in self.iter() {
            if k != key {
                let _ = m.insert(String::from(k), v.clone());
            }
        }
        m
    }
}

/// Equality ignores key order (SHELL-SPEC section 7).
impl PartialEq for Map {
    fn eq(&self, other: &Map) -> bool {
        self.len() == other.len() && self.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}
