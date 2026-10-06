use crate::MediaError;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
pub struct Retained<T> {
    value: T,
    _budget: RetainedBudget,
}

impl<T> Retained<T> {
    fn new(value: T, bytes: usize) -> Result<Arc<Self>, MediaError> {
        let mut budget = RetainedBudget::default();
        budget.reserve(bytes)?;
        Ok(Arc::new(Self {
            value,
            _budget: budget,
        }))
    }
}

impl Retained<Vec<u8>> {
    pub fn frame(value: Vec<u8>) -> Result<Arc<Self>, MediaError> {
        let bytes = value.capacity();
        Self::new(value, bytes)
    }
}

impl Retained<Vec<Vec<u8>>> {
    pub fn frames(value: Vec<Vec<u8>>) -> Result<Arc<Self>, MediaError> {
        let overflow = || MediaError::InvalidConfig("Prepared video size overflow".into());
        let mut bytes = value
            .capacity()
            .checked_mul(std::mem::size_of::<Vec<u8>>())
            .ok_or_else(overflow)?;
        for frame in &value {
            bytes = bytes.checked_add(frame.capacity()).ok_or_else(overflow)?;
        }
        Self::new(value, bytes)
    }
}

impl<T> std::ops::Deref for Retained<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

const MAX_RETAINED_BYTES: usize = 1024 * 1024 * 1024;
static RETAINED_BYTES: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(crate) struct RetainedBudget {
    bytes: usize,
    used: &'static AtomicUsize,
    limit: usize,
}

impl Default for RetainedBudget {
    fn default() -> Self {
        Self {
            bytes: 0,
            used: &RETAINED_BYTES,
            limit: MAX_RETAINED_BYTES,
        }
    }
}

impl RetainedBudget {
    pub(crate) fn reserve(&mut self, bytes: usize) -> Result<(), MediaError> {
        self.used.try_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
            used.checked_add(bytes).filter(|total| *total <= self.limit)
        }).map_err(|_| MediaError::InvalidConfig(
            "Prepared media exceeds the shared 1 GiB memory limit. Shorten videos or reduce widget sizes.".into()
        ))?;
        self.bytes += bytes;
        Ok(())
    }
}

impl Drop for RetainedBudget {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn reservations_preserve_live_assets_and_recover_after_last_owner_drops() {
        static USED: AtomicUsize = AtomicUsize::new(0);
        let mut first = RetainedBudget {
            bytes: 0,
            used: &USED,
            limit: 100,
        };
        first.reserve(60).unwrap();
        let first = Arc::new(Retained {
            value: vec![7u8, 11, 19],
            _budget: first,
        });
        let worker = Arc::clone(&first);
        let mut replacement = RetainedBudget {
            bytes: 0,
            used: &USED,
            limit: 100,
        };
        replacement.reserve(40).unwrap();
        assert!(replacement.reserve(1).is_err());
        drop(first);
        assert_eq!(worker.as_slice(), &[7, 11, 19]);
        assert!(replacement.reserve(1).is_err());
        drop(worker);
        replacement.reserve(60).unwrap();
        assert_eq!(USED.load(Ordering::Relaxed), 100);
        drop(replacement);
        assert_eq!(USED.load(Ordering::Relaxed), 0);
    }
}
