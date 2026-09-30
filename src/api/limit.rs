//! Optional per-key request limits: a token bucket per credential that refills
//! continuously up to one minute's allowance.
use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
    time::{Duration, Instant},
};

pub(super) struct KeyLimits {
    per_minute: Option<u32>,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl KeyLimits {
    pub(super) fn new(per_minute: Option<u32>) -> Self {
        Self {
            per_minute,
            buckets: Mutex::default(),
        }
    }

    /// Spends one request, or returns how long until the key has one again.
    pub(super) fn take(&self, credential: &str) -> Result<(), Duration> {
        let Some(per_minute) = self.per_minute else {
            return Ok(());
        };
        let capacity = f64::from(per_minute);
        let rate = capacity / 60.0;
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let (tokens, last) = buckets
            .entry(credential.to_owned())
            .or_insert((capacity, now));
        *tokens = (*tokens + now.duration_since(*last).as_secs_f64() * rate).min(capacity);
        *last = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - *tokens) / rate))
        }
    }
}
