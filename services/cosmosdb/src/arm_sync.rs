//! Keeps Resource Manager in step with the data plane: a database or
//! container created or deleted through the data plane appears in, or leaves,
//! the account's `sqlDatabases` and `containers` listings.

use emulator_core::arm::{ArmState, Resource};
use emulator_core::Engine;
use serde_json::json;

use crate::accounts::{CONTAINER_TYPE, DATABASE_TYPE};
use crate::data::account_resource_id;

fn insert(engine: &Engine, id: String, name: &str, resource_type: &str) {
    let state = engine.state::<ArmState>();
    let mut state = state.lock();
    if state.resource(&id).is_none() {
        state.insert(Resource {
            id,
            name: name.to_string(),
            resource_type: resource_type.to_string(),
            location: None,
            tags: None,
            kind: None,
            sku: None,
            identity: None,
            properties: json!({"resource": {"id": name}, "options": {}}),
        });
    }
}

fn remove(engine: &Engine, id: &str) {
    let state = engine.state::<ArmState>();
    let mut state = state.lock();
    let children: Vec<String> = state.children(&format!("{id}/containers")).map(|r| r.id.clone()).collect();
    for child in children {
        state.remove(&child);
    }
    state.remove(id);
}

pub(crate) fn database_added(engine: &Engine, account: &str, database: &str) {
    if let Some(account_id) = account_resource_id(engine, account) {
        insert(engine, format!("{account_id}/sqlDatabases/{database}"), database, DATABASE_TYPE);
    }
}

pub(crate) fn database_removed(engine: &Engine, account: &str, database: &str) {
    if let Some(account_id) = account_resource_id(engine, account) {
        remove(engine, &format!("{account_id}/sqlDatabases/{database}"));
    }
}

pub(crate) fn container_added(engine: &Engine, account: &str, database: &str, container: &str) {
    if let Some(account_id) = account_resource_id(engine, account) {
        insert(engine, format!("{account_id}/sqlDatabases/{database}/containers/{container}"), container, CONTAINER_TYPE);
    }
}

pub(crate) fn container_removed(engine: &Engine, account: &str, database: &str, container: &str) {
    if let Some(account_id) = account_resource_id(engine, account) {
        remove(engine, &format!("{account_id}/sqlDatabases/{database}/containers/{container}"));
    }
}
