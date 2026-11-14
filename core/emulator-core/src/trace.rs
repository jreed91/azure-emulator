//! An opt-in record of the operations a test run reached, for checking that
//! every implemented operation is exercised by some test.
//!
//! With `AZURE_EMULATOR_TRACE=<file>` set, each request a service can name
//! appends a line `<plane> <operation> <status>` to the file. Several
//! emulators may share one file: each line is written with a single append.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

use http::StatusCode;

fn file() -> &'static Option<Mutex<std::fs::File>> {
    static FILE: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    FILE.get_or_init(|| {
        let path = std::env::var_os("AZURE_EMULATOR_TRACE").filter(|p| !p.is_empty())?;
        OpenOptions::new().create(true).append(true).open(path).ok().map(Mutex::new)
    })
}

/// Whether a trace is being written.
pub fn enabled() -> bool {
    file().is_some()
}

/// Records that `operation` on `plane` (`arm`, `blob`, `keyvault`, ...)
/// answered with `status`.
pub fn record(plane: &str, operation: &str, status: StatusCode) {
    if let Some(file) = file() {
        let line = format!("{plane} {operation} {}\n", status.as_u16());
        if let Ok(mut file) = file.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }
}
