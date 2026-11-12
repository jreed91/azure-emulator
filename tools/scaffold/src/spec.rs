//! Reading OpenAPI 2.0 documents: discovering versions and resolving `$ref`s.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use serde_json::Value;

/// One `api-version` directory.
pub struct Version {
    pub name: String,
    pub stable: bool,
    pub files: Vec<PathBuf>,
}

/// Finds every `stable/<version>/` and `preview/<version>/` directory under `root`,
/// ordered oldest first (a stable version sorts after the preview of the same date).
pub fn discover_versions(root: &Path) -> Vec<Version> {
    let mut versions = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let stable = dir.file_name().is_some_and(|n| n == "stable");
        let is_track = stable || dir.file_name().is_some_and(|n| n == "preview");
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if !is_track {
                pending.push(path);
                continue;
            }
            let mut files: Vec<PathBuf> = std::fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "json"))
                .collect();
            files.sort();
            if !files.is_empty() {
                versions.push(Version {
                    name: entry.file_name().to_string_lossy().into_owned(),
                    stable,
                    files,
                });
            }
        }
    }
    versions.sort_by(|a, b| (&a.name[..a.name.len().min(10)], a.stable, &a.name)
        .cmp(&(&b.name[..b.name.len().min(10)], b.stable, &b.name)));
    versions
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Loads documents once and follows `$ref`s, including across files.
#[derive(Default)]
pub struct Resolver {
    documents: HashMap<PathBuf, Option<Rc<Value>>>,
}

impl Resolver {
    pub fn load(&mut self, file: &Path) -> Option<Rc<Value>> {
        let key = normalize(file);
        self.documents
            .entry(key.clone())
            .or_insert_with(|| {
                let text = std::fs::read_to_string(&key).ok()?;
                serde_json::from_str(text.trim_start_matches('\u{feff}')).ok().map(Rc::new)
            })
            .clone()
    }

    /// Follows `value` while it is a `$ref`. Returns the file the final value lives
    /// in and the value, or `None` if a reference cannot be followed.
    pub fn resolve(&mut self, file: &Path, value: &Value) -> Option<(PathBuf, Value)> {
        let mut file = file.to_path_buf();
        let mut value = value.clone();
        for _ in 0..32 {
            let Some(reference) = value.get("$ref").and_then(Value::as_str) else {
                return Some((file, value));
            };
            let (target, pointer) = reference.split_once('#').unwrap_or((reference, ""));
            if !target.is_empty() {
                file = normalize(&file.parent()?.join(target));
            }
            let document = self.load(&file)?;
            value = document.pointer(pointer)?.clone();
        }
        None
    }
}

/// The last segment of a `$ref`, e.g. `Sku` for `#/definitions/Sku`.
pub fn ref_name(value: &Value) -> Option<&str> {
    value.get("$ref")?.as_str()?.rsplit('/').next()
}
