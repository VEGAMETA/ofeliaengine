//! Saved simulation state for rollback, replays and desync checks (ADR 0066).

use engine_core::hash::state_hash64;

/// The simulation state of a world at one moment, written by
/// [`PhysicsBackend::save_state`](crate::PhysicsBackend::save_state) and read back by
/// [`PhysicsBackend::restore_state`](crate::PhysicsBackend::restore_state).
///
/// The bytes are the backend's own format: deterministic (the same state gives the same
/// bytes on the same binary), but not portable between backends or engine versions. Keep
/// one snapshot per frame of the rollback window and reuse them: saving into a snapshot
/// whose buffer is large enough does not allocate.
///
/// ```
/// use engine_physics::PhysicsSnapshot;
///
/// // A ring of eight frames for rollback, allocated once.
/// let mut ring: Vec<PhysicsSnapshot> = (0..8).map(|_| PhysicsSnapshot::new()).collect();
/// assert!(ring[0].is_empty());
/// ```
#[derive(Debug, Default)]
pub struct PhysicsSnapshot {
    bytes: Vec<u8>,
}

impl PhysicsSnapshot {
    /// An empty snapshot (restoring it fails).
    pub const fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// An empty snapshot with room for `bytes` bytes, so the first save does not grow it.
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(bytes),
        }
    }

    /// The saved bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Size of the saved state in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Nothing saved yet.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// A 64-bit hash of the saved bytes ([`engine_core::hash::StateHasher`]). Equal states
    /// give equal hashes on the same binary; compare them between peers to detect a
    /// desync. It covers more than
    /// [`PhysicsBackend::state_hash`](crate::PhysicsBackend::state_hash): sleep state,
    /// cached contacts and constraint state too.
    pub fn hash(&self) -> u64 {
        state_hash64(&self.bytes)
    }

    /// The buffer, for backends writing a snapshot. Clear it and write; keep its capacity.
    pub fn buffer_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// Copies `other` into this snapshot, reusing this buffer.
    pub fn copy_from(&mut self, other: &PhysicsSnapshot) {
        self.bytes.clear();
        self.bytes.extend_from_slice(&other.bytes);
    }
}

impl Clone for PhysicsSnapshot {
    fn clone(&self) -> Self {
        Self {
            bytes: self.bytes.clone(),
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.copy_from(source);
    }
}
