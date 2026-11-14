//! The service properties of an account's Blob, Queue, Table and File services:
//! logging, metrics, CORS rules and, for blobs, delete retention and the
//! static website.
//!
//! A Set Service Properties request replaces the top-level elements it names
//! and leaves the others as they were ("If an element is not specified, the
//! existing settings for that functionality are preserved"); Get returns the
//! current document.

use std::collections::BTreeMap;

use emulator_core::Engine;

use crate::files::without_declaration;
use crate::state::StorageState;

const LOGGING: &str = "<Logging><Version>1.0</Version><Read>false</Read><Write>false</Write><Delete>false</Delete><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></Logging>";
const HOUR_METRICS: &str = "<HourMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></HourMetrics>";
const MINUTE_METRICS: &str = "<MinuteMetrics><Version>1.0</Version><Enabled>false</Enabled><RetentionPolicy><Enabled>false</Enabled></RetentionPolicy></MinuteMetrics>";

/// The services that have properties.
#[derive(Clone, Copy)]
pub(crate) enum Service {
    Blob,
    Queue,
    Table,
    File,
}

impl Service {
    fn key(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Queue => "queue",
            Self::Table => "table",
            Self::File => "file",
        }
    }

    /// The properties of a new account, as top-level elements.
    fn defaults(self) -> Vec<String> {
        let mut out: Vec<String> = match self {
            // The File service has no logging.
            Self::File => vec![HOUR_METRICS.into(), MINUTE_METRICS.into()],
            _ => vec![LOGGING.into(), HOUR_METRICS.into(), MINUTE_METRICS.into()],
        };
        out.push("<Cors />".into());
        if let Self::Blob = self {
            out.push("<DeleteRetentionPolicy><Enabled>false</Enabled></DeleteRetentionPolicy>".into());
            out.push("<StaticWebsite><Enabled>false</Enabled></StaticWebsite>".into());
        }
        out
    }
}

/// The name of an element that starts at `text` (just after its `<`).
fn element_name(text: &str) -> &str {
    let end = text.find(|c: char| c.is_whitespace() || c == '>' || c == '/').unwrap_or(text.len());
    &text[..end]
}

/// The top-level elements of a `StorageServiceProperties` document, each as
/// its whole text, or `None` if it is not one.
fn elements(document: &str) -> Option<Vec<(String, String)>> {
    let document = without_declaration(document.trim()).trim();
    let inner = document.strip_prefix("<StorageServiceProperties>")?.strip_suffix("</StorageServiceProperties>")?;
    let mut out = Vec::new();
    let mut rest = inner.trim_start();
    while !rest.is_empty() {
        let after = rest.strip_prefix('<')?;
        let name = element_name(after);
        if name.is_empty() {
            return None;
        }
        let open_end = after.find('>')?;
        let length = if after[..open_end].ends_with('/') {
            // `<Cors />`
            1 + open_end + 1
        } else {
            let close = format!("</{name}>");
            after.find(&close)? + 1 + close.len()
        };
        out.push((name.to_string(), rest[..length].to_string()));
        rest = rest[length..].trim_start();
    }
    Some(out)
}

fn document(elements: impl IntoIterator<Item = String>) -> String {
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><StorageServiceProperties>");
    out.extend(elements);
    out.push_str("</StorageServiceProperties>");
    out
}

/// The service's current properties document.
pub(crate) fn get(stored: &BTreeMap<String, String>, service: Service) -> String {
    stored.get(service.key()).cloned().unwrap_or_else(|| document(service.defaults()))
}

/// Applies a Set Service Properties body, or `Err` if it is not a properties
/// document.
pub(crate) fn set(stored: &mut BTreeMap<String, String>, service: Service, body: &str) -> Result<(), ()> {
    let update = elements(body).ok_or(())?;
    let mut current = elements(&get(stored, service)).unwrap_or_default();
    for (name, text) in update {
        match current.iter_mut().find(|(have, _)| *have == name) {
            Some(slot) => slot.1 = text,
            None => current.push((name, text)),
        }
    }
    stored.insert(service.key().to_string(), document(current.into_iter().map(|(_, text)| text)));
    Ok(())
}

/// The account's current properties document for `service`, or `None` if the
/// account does not exist.
pub(crate) fn read(engine: &Engine, account: &str, service: Service) -> Option<String> {
    let state = engine.state::<StorageState>();
    let state = state.lock();
    state.accounts.get(account).map(|a| get(&a.service_properties, service))
}

/// Applies a Set Service Properties body to the account: `None` if the account
/// does not exist, `Some(Err(()))` if the body is not a properties document.
pub(crate) fn write(engine: &Engine, account: &str, service: Service, body: &str) -> Option<Result<(), ()>> {
    let state = engine.state::<StorageState>();
    let mut state = state.lock();
    state.accounts.get_mut(account).map(|a| set(&mut a.service_properties, service, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_replaces_only_the_elements_it_names() {
        let mut stored = BTreeMap::new();
        let cors = "<Cors><CorsRule><AllowedOrigins>*</AllowedOrigins></CorsRule></Cors>";
        set(&mut stored, Service::Queue, &format!("<?xml version=\"1.0\"?><StorageServiceProperties>{cors}</StorageServiceProperties>")).unwrap();
        let after = get(&stored, Service::Queue);
        assert!(after.contains(cors) && after.contains(LOGGING) && !after.contains("<Cors />"), "{after}");
        // The order of the elements is kept.
        assert!(after.find("<Logging>") < after.find("<Cors>"));
        assert!(set(&mut stored, Service::Queue, "<NotProperties />").is_err());
        assert!(set(&mut stored, Service::Queue, "<StorageServiceProperties><Cors></StorageServiceProperties>").is_err());
        assert!(get(&stored, Service::Blob).contains("<StaticWebsite>"));
        assert!(!get(&stored, Service::File).contains("<Logging>"));
    }
}
