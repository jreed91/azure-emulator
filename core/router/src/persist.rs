//! Keeping the emulator's state across restarts (`--persist`).
//!
//! The state is one JSON snapshot, `state.json` in the emulator's home
//! directory unless `--state-file` names another. It is read at startup,
//! written whenever it changes (checked every few seconds) and again on
//! shutdown. Each write goes to a temporary file that then replaces the
//! snapshot, so a crash mid-write never leaves a half-written snapshot.
//!
//! Persisted: Resource Manager (resources, role assignments, custom roles,
//! deployments), Storage (blobs, queues, tables and their settings), Key
//! Vault (secrets, keys with their material, deleted items), Cosmos DB
//! (databases, containers, items) and Service Bus and Event Hubs keys.
//! Not persisted: messages and events in flight in Service Bus and Event
//! Hubs; their namespaces, entities and settings are.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use emulator_core::Engine;

/// How often a changed state is written.
const INTERVAL: Duration = Duration::from_secs(2);

/// Marks every state a snapshot holds.
pub fn register(engine: &Engine) {
    emulator_core::arm::persist(engine);
    engine.persist::<service_storage::StorageState>("storage");
    engine.persist::<service_keyvault::KeyVaultState>("keyvault");
    engine.persist::<service_cosmosdb::CosmosState>("cosmosdb");
    engine.persist::<service_servicebus::ServiceBusState>("servicebus");
}

/// The snapshot's default place: `state.json` in the emulator's home directory.
pub fn default_path() -> PathBuf {
    crate::tls::home_dir().join("state.json")
}

/// Loads the snapshot at `path`, if there is one. Returns whether it loaded.
/// A snapshot that cannot be read is set aside as `<path>.unreadable`, so it
/// is not overwritten, and the emulator starts empty.
pub fn load(engine: &Engine, path: &Path) -> Result<bool, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let restored = serde_json::from_str(&text).map_err(|e| e.to_string()).and_then(|snapshot| engine.restore(&snapshot));
    match restored {
        Ok(()) => Ok(true),
        Err(error) => {
            let aside = path.with_extension("json.unreadable");
            let _ = std::fs::rename(path, &aside);
            Err(format!("{} could not be loaded ({error}); it was moved to {} and the emulator starts empty", path.display(), aside.display()))
        }
    }
}

/// Writes the snapshot if it differs from `last`; returns what was written.
pub fn save(engine: &Engine, path: &Path, last: Option<&str>) -> Result<Option<String>, String> {
    let snapshot = engine.snapshot()?;
    let text = serde_json::to_string(&snapshot).map_err(|e| e.to_string())?;
    if last == Some(text.as_str()) {
        return Ok(None);
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, &text).map_err(|e| format!("cannot write {}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    Ok(Some(text))
}

/// Writes the snapshot whenever the state changes, until the process ends.
pub async fn keep(engine: Arc<Engine>, path: PathBuf) {
    let mut last = None::<String>;
    loop {
        tokio::time::sleep(INTERVAL).await;
        let engine = engine.clone();
        let path = path.clone();
        let previous = last.clone();
        // Serializing a large store is blocking work.
        match tokio::task::spawn_blocking(move || save(&engine, &path, previous.as_deref())).await {
            Ok(Ok(Some(written))) => last = Some(written),
            Ok(Ok(None)) => {}
            Ok(Err(error)) => eprintln!("warning: saving state failed: {error}"),
            Err(error) => eprintln!("warning: saving state failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snapshot_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("azure-emulator-persist-{}", std::process::id()));
        let path = dir.join("state.json");
        let first = crate::Emulator::new(emulator_core::Config::default());
        register(&first.engine());
        let id = "/subscriptions/s/resourceGroups/g/providers/Microsoft.Storage/storageAccounts/kept";
        first.engine().state::<emulator_core::arm::ArmState>().lock().insert(emulator_core::arm::Resource {
            id: id.into(),
            name: "kept".into(),
            resource_type: "Microsoft.Storage/storageAccounts".into(),
            location: Some("eastus".into()),
            tags: None,
            kind: None,
            sku: None,
            identity: None,
            properties: serde_json::json!({"kept": true}),
        });
        let written = save(&first.engine(), &path, None).unwrap().expect("first save writes");
        assert!(save(&first.engine(), &path, Some(&written)).unwrap().is_none(), "an unchanged state is not rewritten");

        let second = crate::Emulator::new(emulator_core::Config::default());
        register(&second.engine());
        assert!(load(&second.engine(), &path).unwrap());
        assert!(second.engine().state::<emulator_core::arm::ArmState>().lock().resource(id).is_some_and(|r| r.properties["kept"] == true));

        std::fs::write(&path, "{not json").unwrap();
        assert!(load(&second.engine(), &path).is_err());
        assert!(dir.join("state.json.unreadable").exists(), "an unreadable snapshot is set aside");
        assert!(!load(&second.engine(), &path).unwrap(), "and the emulator starts empty");
        let _ = std::fs::remove_dir_all(dir);
    }
}
