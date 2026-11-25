//! The Azure SQL / SQL Server data plane: a TDS endpoint in front of an
//! in-memory engine for a subset of T-SQL.

pub mod ast;
pub mod catalog;
pub mod exec;
pub mod lexer;
pub mod parser;
pub mod tds;
pub mod value;

pub use catalog::SqlState;

#[cfg(test)]
mod tests;

use emulator_core::Engine;
use serde_json::Value as Json;

/// Sets a SQL login's password (creating the login). Setting `sa`'s ends the
/// accept-any-password default.
pub fn set_password(engine: &Engine, login: &str, password: &str) {
    engine.state::<SqlState>().lock().set_login(login, password);
}

/// Splits a script into batches on lines holding only `GO`, as sqlcmd does.
pub fn split_batches(script: &str) -> Vec<String> {
    let mut batches = Vec::new();
    let mut current = String::new();
    for line in script.lines() {
        if line.trim().eq_ignore_ascii_case("go") {
            if !current.trim().is_empty() {
                batches.push(std::mem::take(&mut current));
            }
            current.clear();
        } else {
            current.push_str(line);
            current.push('\n');
        }
    }
    if !current.trim().is_empty() {
        batches.push(current);
    }
    batches
}

/// Applies the `sql` section of a `--config` file:
/// `{ "saPassword", "logins": [{name, password}], "databases": [..], "scripts": [..] }`.
/// Returns one line per change made.
pub fn provision(engine: &Engine, config: &Json) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    let state = engine.state::<SqlState>();
    let mut guard = state.lock();
    if let Some(password) = config.get("saPassword").and_then(Json::as_str) {
        guard.set_login("sa", password);
        done.push("sql: set the password of login sa".to_string());
    }
    for login in config.get("logins").and_then(Json::as_array).into_iter().flatten() {
        let name = login.get("name").and_then(Json::as_str).ok_or("sql.logins[] needs a name")?;
        let password = login.get("password").and_then(Json::as_str).ok_or("sql.logins[] needs a password")?;
        guard.set_login(name, password);
        done.push(format!("sql: login {name}"));
    }
    for database in config.get("databases").and_then(Json::as_array).into_iter().flatten() {
        let name = database
            .as_str()
            .or_else(|| database.get("name").and_then(Json::as_str))
            .ok_or("sql.databases[] holds names")?;
        if guard.database(name).is_none() {
            guard.create_database(name);
        }
        done.push(format!("sql: database {name}"));
    }
    let mut session = exec::Session::new(0, "sa", "master");
    for (i, script) in config.get("scripts").and_then(Json::as_array).into_iter().flatten().enumerate() {
        let script = script.as_str().ok_or("sql.scripts[] holds T-SQL text")?;
        for batch in split_batches(script) {
            let mut events = Vec::new();
            exec::Executor::new(&mut guard, &mut session, &mut events).run_batch(&batch);
            if let Some(exec::Event::Error(e)) = events.iter().find(|e| matches!(e, exec::Event::Error(_))) {
                return Err(format!("sql.scripts[{i}]: {}", e.message));
            }
        }
        done.push(format!("sql: ran script {i}"));
    }
    // A script that leaves a transaction open keeps its changes.
    while session.in_transaction() {
        let _ = exec::Executor::new(&mut guard, &mut session, &mut Vec::new()).commit();
    }
    Ok(done)
}
