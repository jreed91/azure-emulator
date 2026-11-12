//! The single in-memory store shared by every service.
//!
//! Each service defines its own state struct and reaches it through
//! `engine.state::<MyState>()`. One engine exists per emulator instance, so embedded
//! instances are fully isolated from each other.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::Config;

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
}

impl Engine {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            states: Mutex::new(HashMap::new()),
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
}
