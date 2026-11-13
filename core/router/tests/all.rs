//! Cross-service tests: every request goes through the same router the binary serves.

#[path = "all/support.rs"]
mod support;

#[path = "all/arm.rs"]
mod arm;
#[path = "all/entra.rs"]
mod entra;
#[path = "all/scaffold.rs"]
mod scaffold;
#[path = "all/keyvault.rs"]
mod keyvault;
#[path = "all/servicebus.rs"]
mod servicebus;
#[path = "all/storage.rs"]
mod storage;
