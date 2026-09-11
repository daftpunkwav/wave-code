/*!
 * @file LeaseStore
 * @description Single-process leases with fencing tokens.
 *
 * Responsibilities:
 * - Grant exclusive resource holds with expiries.
 * - Fence stale holders with monotonic tokens.
 * - Renew and release holds explicitly.
 * - Expose live holds for inspection and dashboards.
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

    /// Inspect the live hold on a resource at `now_secs`, if any.
    ///
    /// Lapsed holds read as absent even before anyone re-acquires: leases
    /// free on expiry, not on release, so dashboards must never show a
    /// dead holder. Unknown resources also read as absent.
    pub fn holder_at(&self, resource: &str, now_secs: u64) -> Option<&Lease> {
        self.leases
            .get(resource)
            .filter(|lease| lease.expires_at > now_secs)
    }
}

/// Distributed lease backend seam.
///
/// The in-process [`LeaseStore`] is the local frontier; etcd, Redis, or
/// database backends implement this shape over a shared clock later.
/// Method shapes mirror [`LeaseStore`] so drivers swap without rewrites.
pub trait LeaseBackend: Send + Sync {
    /// Acquire a hold or fail when live-held elsewhere.
    fn acquire(
        &mut self,
        resource: String,
        holder: String,
        ttl_secs: u64,
        now_secs: u64,
    ) -> Result<Lease, LeaseError>;

    /// Extend a live hold; false for stale fencing or lapsed holds.
    fn renew(&mut self, lease: &Lease, ttl_secs: u64, now_secs: u64) -> bool;

    /// Release a hold; stale fencing releases nothing.
    fn release(&mut self, lease: &Lease) -> bool;
}

/// Local backend: the single-process store behind the seam.
#[derive(Debug, Default)]
pub struct LocalBackend {
    store: LeaseStore,
}

impl LocalBackend {
    /// Create an empty local backend.
    pub fn new() -> Self {
        Self::default()
    }
}

impl LeaseBackend for LocalBackend {
    fn acquire(
        &mut self,
        resource: String,
        holder: String,
        ttl_secs: u64,
        now_secs: u64,
    ) -> Result<Lease, LeaseError> {
        self.store.acquire(resource, holder, ttl_secs, now_secs)
    }

    fn renew(&mut self, lease: &Lease, ttl_secs: u64, now_secs: u64) -> bool {
        self.store.renew(lease, ttl_secs, now_secs)
    }

    fn release(&mut self, lease: &Lease) -> bool {
        self.store.release(lease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_backend_serves_the_seam() {
        let mut backend = LocalBackend::new();
        let lease = backend
            .acquire("gpu".to_string(), "a".to_string(), 10, 0)
            .unwrap();
        assert!(
            backend
                .acquire("gpu".to_string(), "b".to_string(), 10, 5)
                .is_err()
        );
        assert!(backend.renew(&lease, 10, 5));
        assert!(backend.release(&lease));
        assert!(
            backend
                .acquire("gpu".to_string(), "b".to_string(), 10, 6)
                .is_ok()
        );
    }

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

    #[test]
    fn inspection_hides_lapsed_holds() {
        let mut store = LeaseStore::new();
        assert_eq!(store.holder_at("gpu", 0), None);
        let lease = store.acquire("gpu", "a", 10, 0).unwrap();
        assert_eq!(store.holder_at("gpu", 5), Some(&lease));
        // Lapsed at t=10 reads as absent even before re-acquire.
        assert_eq!(store.holder_at("gpu", 10), None);
        assert_eq!(store.holder_at("gpu", 100), None);
    }
}
