//! The component registry.
//!
//! Holds the compiled component currently active in each slot. Swapping is a
//! pointer replacement: calls already in flight keep the `Arc` they started
//! with and finish on the old code, while the next call picks up the new one.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use wasmtime::component::Component;
use wasmtime::Engine;

use crate::slot::Slot;

pub struct LoadedComponent {
    pub slot: Slot,
    pub revision: u64,
    pub component: Component,
}

#[derive(Default)]
pub struct Loader {
    slots: RwLock<HashMap<Slot, Arc<LoadedComponent>>>,
}

impl Loader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Compiles a `.wasm` file. Returns an error if it is not a valid component
    /// for this engine — the first gate a candidate build must pass.
    pub fn compile(
        engine: &Engine,
        slot: &Slot,
        revision: u64,
        path: &Path,
    ) -> Result<Arc<LoadedComponent>> {
        let component = Component::from_file(engine, path)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("compiling {} from {}", slot, path.display()))?;
        Ok(Arc::new(LoadedComponent {
            slot: slot.clone(),
            revision,
            component,
        }))
    }

    pub fn get(&self, slot: &Slot) -> Option<Arc<LoadedComponent>> {
        self.slots.read().ok()?.get(slot).cloned()
    }

    pub fn install(&self, component: Arc<LoadedComponent>) {
        if let Ok(mut slots) = self.slots.write() {
            slots.insert(component.slot.clone(), component);
        }
    }

    pub fn remove(&self, slot: &Slot) {
        if let Ok(mut slots) = self.slots.write() {
            slots.remove(slot);
        }
    }

    /// Every active slot and its revision, for `/admin` and the agent's own
    /// `history` view.
    pub fn active(&self) -> Vec<(Slot, u64)> {
        let Ok(slots) = self.slots.read() else {
            return Vec::new();
        };
        let mut out: Vec<(Slot, u64)> = slots
            .values()
            .map(|c| (c.slot.clone(), c.revision))
            .collect();
        out.sort_by_key(|(s, _)| s.key());
        out
    }

    pub fn tools(&self) -> Vec<Slot> {
        let Ok(slots) = self.slots.read() else {
            return Vec::new();
        };
        let mut out: Vec<Slot> = slots
            .keys()
            .filter(|s| matches!(s, Slot::Tool(_)))
            .cloned()
            .collect();
        out.sort_by_key(|s| s.key());
        out
    }
}
