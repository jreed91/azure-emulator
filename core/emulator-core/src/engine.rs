//! The single in-memory store shared by every service.
//!
//! Each service defines its own state struct and reaches it through
//! `engine.state::<MyState>()`. One engine exists per emulator instance, so embedded
//! instances are fully isolated from each other.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::Config;

/// The version of the snapshot format; a snapshot of another version is not loaded.
const SNAPSHOT_VERSION: u64 = 1;

type Saver = Box<dyn Fn(&Engine) -> Result<Value, String> + Send + Sync>;
type Loader = Box<dyn Fn(&Engine, Value) -> Result<(), String> + Send + Sync>;

/// Clone-cheap handle to one service's state.
pub struct ServiceState<T>(Arc<Mutex<T>>);

impl<T> Clone for ServiceState<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> ServiceState<T> {
    /// Locks the state. A lock poisoned by a panicking handler is recovered so one bad
    /// request cannot wedge the emulator.
    pub fn lock(&self) -> MutexGuard<'_, T> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

pub struct Engine {
    config: Config,
    states: Mutex<HashMap<TypeId, Box<dyn Any + Send + Sync>>>,
    /// The states that survive a restart, by name, when state is persisted.
    persisted: Mutex<Vec<(&'static str, Saver, Loader)>>,
}

impl Engine {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            states: Mutex::new(HashMap::new()),
            persisted: Mutex::new(Vec::new()),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Returns the state of type `T`, creating it on first use.
    pub fn state<T: Default + Send + 'static>(&self) -> ServiceState<T> {
        let mut states = self.states.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = states
            .entry(TypeId::of::<T>())
            .or_insert_with(|| Box::new(ServiceState(Arc::new(Mutex::new(T::default())))));
        entry
            .downcast_ref::<ServiceState<T>>()
            .expect("state stored under its own TypeId")
            .clone()
    }

    /// Marks the state of type `T` as one a snapshot holds, under `name`.
    pub fn persist<T: Default + Send + Serialize + DeserializeOwned + 'static>(&self, name: &'static str) {
        let save: Saver = Box::new(|engine| serde_json::to_value(&*engine.state::<T>().lock()).map_err(|e| e.to_string()));
        let load: Loader = Box::new(|engine, value| {
            let restored: T = serde_json::from_value(value).map_err(|e| e.to_string())?;
            *engine.state::<T>().lock() = restored;
            Ok(())
        });
        let mut persisted = self.persisted.lock().unwrap_or_else(PoisonError::into_inner);
        persisted.retain(|(existing, _, _)| *existing != name);
        persisted.push((name, save, load));
    }

    /// Every persisted state, as one JSON document.
    pub fn snapshot(&self) -> Result<Value, String> {
        let persisted = self.persisted.lock().unwrap_or_else(PoisonError::into_inner);
        let mut states = Map::new();
        for (name, save, _) in persisted.iter() {
            states.insert(name.to_string(), save(self).map_err(|e| format!("{name}: {e}"))?);
        }
        Ok(json!({"version": SNAPSHOT_VERSION, "states": states}))
    }

    /// Replaces the persisted states with a snapshot's. States the snapshot
    /// does not hold are left as they are. Nothing is replaced unless every
    /// state in it can be read.
    pub fn restore(&self, snapshot: &Value) -> Result<(), String> {
        let version = snapshot.get("version").and_then(Value::as_u64);
        if version != Some(SNAPSHOT_VERSION) {
            return Err(format!("the snapshot is version {version:?}; this emulator reads version {SNAPSHOT_VERSION}"));
        }
        let states = snapshot.get("states").and_then(Value::as_object).ok_or("the snapshot has no states")?;
        let persisted = self.persisted.lock().unwrap_or_else(PoisonError::into_inner);
        // Each state is loaded into a scratch engine first, so a state that
        // cannot be read leaves this one untouched.
        let scratch = Engine::new(self.config.clone());
        for (name, _, load) in persisted.iter() {
            if let Some(value) = states.get(*name) {
                load(&scratch, value.clone()).map_err(|e| format!("{name}: {e}"))?;
            }
        }
        for (name, _, load) in persisted.iter() {
            if let Some(value) = states.get(*name) {
                load(self, value.clone()).map_err(|e| format!("{name}: {e}"))?;
            }
        }
        Ok(())
    }
}
