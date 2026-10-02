//! Known price versions. A reservation or settlement against an unknown version is refused.
use std::collections::BTreeSet;

#[derive(Debug, Clone, Default)]
pub struct PriceBook {
    current: Option<String>,
    known: BTreeSet<String>,
}

impl PriceBook {
    /// `current` is used by the plain `Budget::reserve` entry point and is implicitly known.
    pub fn new(current: Option<String>, known: impl IntoIterator<Item = String>) -> Self {
        let mut known: BTreeSet<String> = known.into_iter().collect();
        if let Some(c) = &current {
            known.insert(c.clone());
        }
        Self { current, known }
    }

    pub fn current(&self) -> Option<&str> {
        self.current.as_deref()
    }

    pub fn is_known(&self, version: &str) -> bool {
        !version.is_empty() && self.known.contains(version)
    }
}
