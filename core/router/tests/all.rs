//! Cross-service tests: every request goes through the same router the binary serves.

#[path = "all/support.rs"]
mod support;

#[path = "all/arm.rs"]
mod arm;
#[path = "all/authorization.rs"]
mod authorization;
#[path = "all/deployment_validation.rs"]
mod deployment_validation;
#[path = "all/deployments.rs"]
mod deployments;
#[path = "all/entra.rs"]
mod entra;
#[path = "all/scaffold.rs"]
mod scaffold;
#[path = "all/keyvault.rs"]
mod keyvault;
#[path = "all/servicebus.rs"]
mod servicebus;
#[path = "all/spec_examples.rs"]
mod spec_examples;
#[path = "all/storage.rs"]
mod storage;
#[path = "all/template_functions.rs"]
mod template_functions;
