//! Structural lock order for shared capture state: durability before mutex.
use crate::vault_capture::CaptureStore;
use std::{
    ops::{Deref, DerefMut},
    sync::{Mutex, MutexGuard},
};

pub(crate) struct CaptureAccess(Mutex<CaptureStore>);
#[cfg(not(test))]
type DurabilityGuard = Option<crate::cell_durability::CellDurabilityWriteGuard>;
#[cfg(test)]
type DurabilityGuard = Option<std::sync::RwLockReadGuard<'static, ()>>;

pub(crate) struct CaptureGuard<'a> {
    // Field order releases the mutex before releasing the durability guard.
    store: MutexGuard<'a, CaptureStore>,
    _durability: DurabilityGuard,
}
impl CaptureAccess {
    pub(crate) fn new(store: CaptureStore) -> Self {
        Self(Mutex::new(store))
    }
    pub(crate) fn lock(&self) -> Result<CaptureGuard<'_>, String> {
        let durability = crate::cell_durability::write_guard();
        let store = self.0.lock().map_err(|_| "capture store poisoned")?;
        Ok(CaptureGuard {
            store,
            _durability: durability,
        })
    }
    pub(crate) fn read<R>(
        &self,
        f: impl FnOnce(&CaptureStore) -> Result<R, String>,
    ) -> Result<R, String> {
        let store = self.0.lock().map_err(|_| "capture store poisoned")?;
        f(&store)
    }
}
impl Deref for CaptureGuard<'_> {
    type Target = CaptureStore;
    fn deref(&self) -> &CaptureStore {
        &self.store
    }
}
impl DerefMut for CaptureGuard<'_> {
    fn deref_mut(&mut self) -> &mut CaptureStore {
        &mut self.store
    }
}
