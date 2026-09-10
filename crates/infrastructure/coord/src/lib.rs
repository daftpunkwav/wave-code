/*!
 * @file LeaseStore
 * @description Single-process leases with fencing tokens.
 *
 * Responsibilities:
 * - Grant exclusive resource holds with expiries.
 * - Fence stale holders with monotonic tokens.
 * - Renew and release holds explicitly.
 *
 * This module must not depend on: any other workspace crate. The store
 * is the single-process frontier: distributed backends implement the
 * same shape over a shared clock later.
 */

//! Coordination primitives with explicit clocks and fencing.

/// One granted lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// Guarded resource name.
    pub resource: String,
    /// Current holder identity.
    pub holder: String,
    /// Fencing token, strictly increasing per resource.
    pub fencing: u64,
    /// Epoch seconds at which the hold lapses.
    pub expires_at: u64,
}

/// Lease failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    /// Another live holder owns the resource.
    #[error("resource held by another live holder: {0}")]
    Held(String),
}

/// Exclusive lease store.
#[derive(Debug, Default)]
pub struct LeaseStore {
    leases: std::collections::HashMap<String, Lease>,
    fencing: u64,
}

impl LeaseStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire a hold for `ttl_secs` from `now_secs`.
    ///
    /// Succeeds when no lease exists or the live one expired; a live
    /// holder blocks everyone else until it lapses or releases.
    pub fn acquire(
        &mut self,
        resource: impl Into<String>,
        holder: impl Into<String>,
        ttl_secs: u64,
        now_secs: u64,
    ) -> Result<Lease, LeaseError> {
        let resource = resource.into();
        if let Some(lease) = self.leases.get(&resource)
            && lease.expires_at > now_secs
        {
            return Err(LeaseError::Held(resource));
        }
        self.fencing += 1;
        let lease = Lease {
            resource: resource.clone(),
            holder: holder.into(),
            fencing: self.fencing,
            expires_at: now_secs + ttl_secs,
        };
        self.leases.insert(resource, lease.clone());
        Ok(lease)
    }

    /// Extend a live hold; false for stale fencing or unknown resources.
    pub fn renew(&mut self, lease: &Lease, ttl_secs: u64, now_secs: u64) -> bool {
        match self.leases.get(&lease.resource) {
            Some(live) if live.fencing == lease.fencing && live.expires_at > now_secs => {
                let mut renewed = live.clone();
                renewed.expires_at = now_secs + ttl_secs;
                self.leases.insert(lease.resource.clone(), renewed);
                true
            }
            _ => false,
        }
    }

    /// Release a hold; stale fencing tokens release nothing.
    pub fn release(&mut self, lease: &Lease) -> bool {
        match self.leases.get(&lease.resource) {
            Some(live) if live.fencing == lease.fencing => {
                self.leases.remove(&lease.resource);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_holders_block_until_expiry_or_release() {
        let mut store = LeaseStore::new();
        let first = store.acquire("gpu", "a", 10, 0).unwrap();
        assert!(store.acquire("gpu", "b", 10, 5).is_err());
        // Stale fencing renews nothing.
        let stale = Lease {
            fencing: 999,
            ..first.clone()
        };
        assert!(!store.renew(&stale, 10, 5));
        assert!(store.renew(&first, 10, 5));
        assert!(store.release(&first));
        let second = store.acquire("gpu", "b", 10, 6).unwrap();
        // Fencing strictly increases across generations.
        assert!(second.fencing > first.fencing);
        // Expiry frees without release.
        assert!(store.acquire("gpu", "c", 10, 100).is_ok());
    }
}
