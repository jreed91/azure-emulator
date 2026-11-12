//! Building a provider's model from its documents: which resource types exist, in
//! which versions, and what every operation does in Resource Manager terms.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::spec::{Resolver, Version};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Put,
    Get,
    Head,
    Patch,
    Delete,
    List,
    ListInSubscription,
    Action(String),
    ProviderAction(String),
    Other,
}

pub struct Operation {
    pub id: String,
    pub method: String,
    pub path: String,
    pub kind: Kind,
    /// Where the definition came from, for resolving its schemas.
    pub file: PathBuf,
    pub definition: Value,
    pub version: String,
}

#[derive(Default)]
pub struct ResourceType {
    /// Canonical casing, e.g. `Microsoft.Storage/storageAccounts`.
    pub name: String,
    pub versions: BTreeSet<String>,
    /// Keyed by (lowercased path, method); the newest definition wins.
    pub operations: BTreeMap<(String, String), Operation>,
}

pub struct Provider {
    pub namespace: String,
    /// Keyed by lowercased type name.
    pub types: BTreeMap<String, ResourceType>,
    /// Operations outside any resource type.
    pub operations: BTreeMap<(String, String), Operation>,
}

fn is_parameter(segment: &str) -> bool {
    segment.starts_with('{')
}

/// Classifies `method path`. Returns the resource type's segments (empty for a
/// provider-level operation) and the kind, or `None` if the path is not this
/// provider's.
fn classify(namespace: &str, method: &str, path: &str) -> Option<(Vec<String>, Kind)> {
    let path = path.split('?').next().unwrap_or(path);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let at = (0..segments.len().saturating_sub(1)).rev().find(|&i| {
        segments[i].eq_ignore_ascii_case("providers") && segments[i + 1].eq_ignore_ascii_case(namespace)
    })?;
    let (scope, rest) = (&segments[..at], &segments[at + 2..]);
    let literal = |index: usize, name: &str| scope.get(index).is_some_and(|s| s.eq_ignore_ascii_case(name));
    let in_subscription = scope.len() == 2 && literal(0, "subscriptions") && is_parameter(scope[1]);
    let in_group = scope.len() == 4
        && literal(0, "subscriptions")
        && is_parameter(scope[1])
        && literal(2, "resourcegroups")
        && is_parameter(scope[3]);
    let provider_level = |kind| Some((Vec::new(), kind));
    // Type segments sit at even positions and must be literals.
    if rest.is_empty() || rest.iter().step_by(2).any(|s| is_parameter(s)) {
        return provider_level(Kind::Other);
    }
    let types = |upto: usize| rest[..upto].iter().step_by(2).map(|s| s.to_string()).collect::<Vec<_>>();
    let count = rest.len();
    if in_group && count.is_multiple_of(2) {
        let kind = match method {
            "put" => Kind::Put,
            "get" => Kind::Get,
            "head" => Kind::Head,
            "patch" => Kind::Patch,
            "delete" => Kind::Delete,
            _ => Kind::Other,
        };
        return Some((types(count), kind));
    }
    if in_group {
        return match method {
            "get" => Some((types(count), Kind::List)),
            "post" if count >= 3 => Some((types(count - 1), Kind::Action(rest[count - 1].to_string()))),
            _ => provider_level(Kind::Other),
        };
    }
    if in_subscription && count == 1 {
        return match method {
            "get" => Some((types(1), Kind::ListInSubscription)),
            "post" => provider_level(Kind::ProviderAction(rest[0].to_string())),
            _ => provider_level(Kind::Other),
        };
    }
    provider_level(Kind::Other)
}

pub fn build(namespace: &str, versions: &[Version], resolver: &mut Resolver) -> Provider {
    let mut provider = Provider {
        namespace: namespace.to_string(),
        types: BTreeMap::new(),
        operations: BTreeMap::new(),
    };
    // Oldest first, so the newest definition of each operation is the one kept.
    for version in versions {
        for file in &version.files {
            let Some(document) = resolver.load(file) else {
                eprintln!("warning: skipping unreadable document {}", file.display());
                continue;
            };
            for section in ["paths", "x-ms-paths"] {
                let Some(paths) = document.get(section).and_then(Value::as_object) else {
                    continue;
                };
                for (path, item) in paths {
                    for method in ["get", "put", "post", "patch", "delete", "head"] {
                        let Some(definition) = item.get(method) else {
                            continue;
                        };
                        let Some((type_segments, kind)) = classify(namespace, method, path) else {
                            continue;
                        };
                        let operation = Operation {
                            id: definition
                                .get("operationId")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            method: method.to_ascii_uppercase(),
                            path: path.clone(),
                            kind,
                            file: file.clone(),
                            definition: definition.clone(),
                            version: version.name.clone(),
                        };
                        let key = (path.to_ascii_lowercase(), method.to_string());
                        if type_segments.is_empty() {
                            provider.operations.insert(key, operation);
                            continue;
                        }
                        let name = format!("{namespace}/{}", type_segments.join("/"));
                        let entry = provider.types.entry(name.to_ascii_lowercase()).or_default();
                        entry.name = name;
                        entry.versions.insert(version.name.clone());
                        entry.operations.insert(key, operation);
                    }
                }
            }
        }
    }
    provider
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUP: &str = "/subscriptions/{subscriptionId}/resourceGroups/{rg}/providers/Microsoft.Storage";

    fn kind(method: &str, path: &str) -> (String, Kind) {
        let (types, kind) = classify("Microsoft.Storage", method, path).expect("this provider's path");
        (types.join("/"), kind)
    }

    #[test]
    fn resource_paths() {
        let account = format!("{GROUP}/storageAccounts/{{accountName}}");
        assert_eq!(kind("put", &account), ("storageAccounts".into(), Kind::Put));
        assert_eq!(kind("delete", &account), ("storageAccounts".into(), Kind::Delete));
        assert_eq!(kind("get", &format!("{GROUP}/storageAccounts")), ("storageAccounts".into(), Kind::List));
        assert_eq!(
            kind("post", &format!("{account}/listKeys")),
            ("storageAccounts".into(), Kind::Action("listKeys".into()))
        );
        // A literal name ("default") is still a name, and nesting carries through.
        assert_eq!(
            kind("put", &format!("{account}/blobServices/default/containers/{{c}}")),
            ("storageAccounts/blobServices/containers".into(), Kind::Put)
        );
        assert_eq!(
            kind("post", &format!("{account}/blobServices/default/containers/{{c}}/lease")),
            ("storageAccounts/blobServices/containers".into(), Kind::Action("lease".into()))
        );
    }

    #[test]
    fn subscription_and_other_scopes() {
        let subscription = "/subscriptions/{subscriptionId}/providers/microsoft.storage";
        assert_eq!(
            kind("get", &format!("{subscription}/storageAccounts")),
            ("storageAccounts".into(), Kind::ListInSubscription)
        );
        assert_eq!(
            kind("post", &format!("{subscription}/checkNameAvailability")),
            (String::new(), Kind::ProviderAction("checkNameAvailability".into()))
        );
        assert_eq!(kind("get", "/providers/Microsoft.Storage/operations"), (String::new(), Kind::Other));
        assert_eq!(
            kind("get", &format!("{subscription}/locations/{{location}}/usages")),
            (String::new(), Kind::Other)
        );
        assert!(classify("Microsoft.Storage", "get", "/subscriptions/{id}/providers/Microsoft.Other/things").is_none());
    }
}
