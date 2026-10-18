//! Lock-free parameter value store shared by the GUI, main, and audio threads.
//!
//! Values are stored as `f64` bit patterns in `AtomicU64` slots indexed by the
//! plugin parameter list order. Lookups by CLAP param id use an immutable map
//! built once at construction — no locks on the audio path.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Concurrent parameter store. Safe to share via `Arc` across threads.
pub struct ParamStore {
    values: Box<[AtomicU64]>,
    ids: Box<[u32]>,
    id_to_idx: HashMap<u32, usize>,
}

impl ParamStore {
    /// Build a store from `(id, initial_value)` pairs. Duplicate ids keep the
    /// first occurrence.
    #[must_use]
    pub fn new(initial: impl IntoIterator<Item = (u32, f64)>) -> Arc<Self> {
        let mut ids = Vec::new();
        let mut values = Vec::new();
        let mut id_to_idx = HashMap::new();

        for (id, value) in initial {
            if let std::collections::hash_map::Entry::Vacant(e) = id_to_idx.entry(id) {
                e.insert(ids.len());
                ids.push(id);
                values.push(AtomicU64::new(value.to_bits()));
            }
        }

        Arc::new(Self {
            values: values.into_boxed_slice(),
            ids: ids.into_boxed_slice(),
            id_to_idx,
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ids.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    #[must_use]
    pub fn get(&self, id: u32) -> Option<f64> {
        let &idx = self.id_to_idx.get(&id)?;
        Some(f64::from_bits(self.values[idx].load(Ordering::Acquire)))
    }

    #[must_use]
    pub fn get_or(&self, id: u32, default: f64) -> f64 {
        self.get(id).unwrap_or(default)
    }

    /// Atomically store a value. Unknown ids are ignored (returns `false`).
    #[allow(clippy::must_use_candidate)]
    pub fn set(&self, id: u32, value: f64) -> bool {
        let Some(&idx) = self.id_to_idx.get(&id) else {
            return false;
        };
        self.values[idx].store(value.to_bits(), Ordering::Release);
        true
    }

    /// Snapshot all `(id, value)` pairs. Allocates — not for the audio thread.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(u32, f64)> {
        self.ids
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, f64::from_bits(self.values[i].load(Ordering::Acquire))))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_roundtrip() {
        let store = ParamStore::new([(0, 10.0), (1, 20.0)]);
        assert_eq!(store.get(0), Some(10.0));
        assert!(store.set(1, 42.5));
        assert_eq!(store.get(1), Some(42.5));
        assert!(!store.set(99, 1.0));
    }
}
