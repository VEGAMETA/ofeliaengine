//! Resources: singletons stored in the world next to entities (time, input, settings).

use core::any::{Any, TypeId};
use core::cell::UnsafeCell;

/// A resource slot. Shared across threads only by the system scheduler, which gives each
/// running system either shared or exclusive access according to its declared accesses.
pub(crate) struct Slot {
    pub(crate) type_id: TypeId,
    pub(crate) value: UnsafeCell<Option<Box<dyn Any + Send + Sync>>>,
}

// SAFETY: values are `Send + Sync`; mutation through a shared reference only happens in
// the scheduler, under its access checks (one writer, no concurrent readers).
unsafe impl Sync for Slot {}

/// Resource storage, indexed by a dense id per type.
#[derive(Default)]
pub(crate) struct Resources {
    pub(crate) slots: Vec<Slot>,
}

impl Resources {
    pub(crate) fn id_of<T: 'static>(&self) -> Option<usize> {
        let t = TypeId::of::<T>();
        self.slots.iter().position(|s| s.type_id == t)
    }

    pub(crate) fn register<T: Send + Sync + 'static>(&mut self) -> usize {
        if let Some(id) = self.id_of::<T>() {
            return id;
        }
        self.slots.push(Slot {
            type_id: TypeId::of::<T>(),
            value: UnsafeCell::new(None),
        });
        self.slots.len() - 1
    }

    /// # Safety
    ///
    /// No `&mut` to this slot's value is alive.
    pub(crate) unsafe fn get<T: 'static>(&self, id: usize) -> Option<&T> {
        // SAFETY: guaranteed by the caller.
        unsafe { (*self.slots.get(id)?.value.get()).as_ref()?.downcast_ref() }
    }

    /// # Safety
    ///
    /// No other reference to this slot's value is alive.
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn get_mut<T: 'static>(&self, id: usize) -> Option<&mut T> {
        // SAFETY: guaranteed by the caller.
        unsafe { (*self.slots.get(id)?.value.get()).as_mut()?.downcast_mut() }
    }
}
