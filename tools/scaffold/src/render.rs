//! Writing a provider's scaffold: `src/scaffold.rs`, `CONTRACT.md`, and — the first
//! time — the crate and its wiring.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::Value;

use crate::model::{Kind, Operation, Provider, ResourceType};
use crate::spec::{ref_name, Resolver};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn kind_literal(kind: &Kind) -> String {
    match kind {
        Kind::Action(name) => format!("Action({name:?})"),
        Kind::ProviderAction(name) => format!("ProviderAction({name:?})"),
        other => format!("{other:?}"),
    }
}

fn operation_literal(operation: &Operation) -> String {
    format!(
        "Op {{ id: {:?}, method: {:?}, path: {:?}, kind: {} }}",
        operation.id,
        operation.method,
        operation.path,
        kind_literal(&operation.kind)
    )
}

/// A raw string literal holding `text`, with enough `#`s that it cannot end early.
fn raw_literal(text: &str) -> String {
    let mut hashes = String::from("#");
    while text.contains(&format!("\"{hashes}")) {
        hashes.push('#');
    }
    format!("r{hashes}\"{text}\"{hashes}")
}

/// The schema a scaffolded type is validated against: the rules for its name
/// and a compact form of its `PUT` body, as JSON. `None` without a `PUT`.
fn type_schema(resolver: &mut Resolver, kind: &ResourceType) -> Option<String> {
    let put = kind.operations.values().find(|o| o.kind == Kind::Put)?;
    let parameters: Vec<Value> = put
        .definition
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| resolver.resolve(&put.file, p).map(|(_, v)| v))
        .collect();
    let mut out = serde_json::json!({});
    let name_parameter = put.path.rsplit('/').next().and_then(|s| s.strip_prefix('{')?.strip_suffix('}'));
    if let Some(parameter) = parameters
        .iter()
        .find(|p| name_parameter.is_some() && p.get("name").and_then(Value::as_str) == name_parameter)
    {
        let mut rules = serde_json::Map::new();
        for key in ["minLength", "maxLength", "pattern"] {
            if let Some(value) = parameter.get(key) {
                rules.insert(key.into(), value.clone());
            }
        }
        out["name"] = Value::Object(rules);
    }
    let body = parameters.iter().find(|p| p.get("in").and_then(Value::as_str) == Some("body"))?;
    let schema = body.get("schema").cloned().unwrap_or(Value::Null);
    out["body"] = compact(resolver, &put.file, &schema, 0);
    Some(out.to_string())
}

/// Merges `extra` (a compacted object schema) into `into`.
fn merge_object(into: &mut serde_json::Map<String, Value>, extra: Value) {
    let Value::Object(extra) = extra else { return };
    for (key, value) in extra {
        match (key.as_str(), into.get_mut(&key)) {
            ("properties", Some(Value::Object(existing))) => {
                if let Value::Object(more) = value {
                    existing.extend(more);
                }
            }
            ("required", Some(Value::Array(existing))) => {
                if let Value::Array(more) = value {
                    existing.extend(more);
                }
            }
            _ => {
                into.insert(key, value);
            }
        }
    }
}

/// The subset of an OpenAPI schema the emulator validates requests against:
/// types, enums (when the spec makes them closed), bounds, patterns, required
/// and read-only members, defaults, and members fixed at creation
/// (`x-ms-mutability` without `update`). `$ref` and `allOf` are flattened.
fn compact(resolver: &mut Resolver, file: &Path, schema: &Value, depth: usize) -> Value {
    let mut out = serde_json::Map::new();
    if depth > 8 {
        return Value::Object(out);
    }
    let Some((file, schema)) = resolver.resolve(file, schema) else {
        return Value::Object(out);
    };
    for parent in schema.get("allOf").and_then(Value::as_array).into_iter().flatten() {
        let inherited = compact(resolver, &file, parent, depth + 1);
        merge_object(&mut out, inherited);
    }
    for key in ["type", "format", "pattern", "minLength", "maxLength", "minimum", "maximum", "minItems", "maxItems", "default", "readOnly"] {
        if let Some(value) = schema.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    if let Some(values) = schema.get("enum") {
        // An enum the spec models as a string is open: services add values.
        let open = schema.pointer("/x-ms-enum/modelAsString").and_then(Value::as_bool) != Some(false);
        if !open {
            out.insert("enum".into(), values.clone());
        }
    }
    if let Some(mutability) = schema.get("x-ms-mutability").and_then(Value::as_array) {
        let can = |what: &str| mutability.iter().any(|m| m == what);
        if can("create") && !can("update") {
            out.insert("createOnly".into(), Value::Bool(true));
        }
    }
    if let Some(members) = schema.get("properties").and_then(Value::as_object) {
        let mut compacted = serde_json::Map::new();
        for (name, member) in members {
            let mut value = compact(resolver, &file, member, depth + 1);
            // Flags written beside a `$ref` apply to the member, not the target.
            for key in ["readOnly", "x-ms-mutability", "default"] {
                if let Some(flag) = member.get(key) {
                    if key == "x-ms-mutability" {
                        let list = flag.as_array().cloned().unwrap_or_default();
                        if list.iter().any(|m| m == "create") && !list.iter().any(|m| m == "update") {
                            value["createOnly"] = Value::Bool(true);
                        }
                    } else {
                        value[key] = flag.clone();
                    }
                }
            }
            compacted.insert(name.clone(), value);
        }
        let mut wrapper = serde_json::Map::new();
        wrapper.insert("properties".into(), Value::Object(compacted));
        merge_object(&mut out, Value::Object(wrapper));
        out.entry("type").or_insert(Value::String("object".into()));
    }
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let mut wrapper = serde_json::Map::new();
        wrapper.insert("required".into(), Value::Array(required.clone()));
        merge_object(&mut out, Value::Object(wrapper));
    }
    if let Some(items) = schema.get("items") {
        out.insert("items".into(), compact(resolver, &file, items, depth + 1));
    }
    if schema.get("additionalProperties").is_some_and(|a| a != &Value::Bool(false)) {
        out.insert("map".into(), Value::Bool(true));
    }
    // A polymorphic base: subtypes add members this schema cannot list.
    if schema.get("discriminator").is_some() {
        out.insert("open".into(), Value::Bool(true));
    }
    Value::Object(out)
}

fn scaffold_source(provider: &Provider) -> String {
    let mut resolver = Resolver::default();
    let mut out = String::new();
    let _ = writeln!(out, "// @generated by emulator-scaffold from the {} OpenAPI specification.", provider.namespace);
    out.push_str("// Do not edit: rerun the generator instead. See tools/scaffold.\n\n");
    out.push_str("use emulator_core::arm::OperationKind::*;\n");
    out.push_str("use emulator_core::arm::{ScaffoldedOperation as Op, ScaffoldedProvider, ScaffoldedType};\n\n");
    out.push_str("#[rustfmt::skip]\npub static PROVIDER: ScaffoldedProvider = ScaffoldedProvider {\n");
    let _ = writeln!(out, "    namespace: {:?},", provider.namespace);
    out.push_str("    types: &[\n");
    for kind in provider.types.values() {
        out.push_str("        ScaffoldedType {\n");
        let _ = writeln!(out, "            resource_type: {:?},", kind.name);
        let versions: Vec<String> = kind.versions.iter().map(|v| format!("{v:?}")).collect();
        let _ = writeln!(out, "            api_versions: &[{}],", versions.join(", "));
        match type_schema(&mut resolver, kind) {
            Some(schema) => {
                let _ = writeln!(out, "            schema: Some({}),", raw_literal(&schema));
            }
            None => out.push_str("            schema: None,\n"),
        }
        out.push_str("            operations: &[\n");
        for operation in kind.operations.values() {
            let _ = writeln!(out, "                {},", operation_literal(operation));
        }
        out.push_str("            ],\n        },\n");
    }
    out.push_str("    ],\n    provider_operations: &[\n");
    for operation in provider.operations.values() {
        let _ = writeln!(out, "        {},", operation_literal(operation));
    }
    out.push_str("    ],\n};\n");
    out
}

/// One line of Markdown table text: no newlines or cell separators, clipped.
fn cell(text: &str) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ").replace('|', "\\|");
    let sentence = flat.split_once(". ").map(|(first, _)| format!("{first}.")).unwrap_or(flat);
    match sentence.char_indices().nth(200) {
        Some((at, _)) => format!("{} …", &sentence[..at]),
        None => sentence,
    }
}

struct Member {
    name: String,
    raw: Value,
    resolved: Value,
    file: std::path::PathBuf,
    required: bool,
}

/// The members of an object schema, following `$ref` and `allOf`.
fn members(resolver: &mut Resolver, file: &Path, schema: &Value, depth: usize) -> Vec<Member> {
    let mut out: Vec<Member> = Vec::new();
    let Some((file, schema)) = resolver.resolve(file, schema).filter(|_| depth < 8) else {
        return out;
    };
    for parent in schema.get("allOf").and_then(Value::as_array).into_iter().flatten() {
        out.extend(members(resolver, &file, parent, depth + 1));
    }
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for (name, raw) in schema.get("properties").and_then(Value::as_object).into_iter().flatten() {
        let (member_file, resolved) = resolver
            .resolve(&file, raw)
            .unwrap_or_else(|| (file.clone(), raw.clone()));
        out.retain(|m| m.name != *name);
        out.push(Member {
            name: name.clone(),
            raw: raw.clone(),
            resolved,
            file: member_file,
            required: required.contains(&name.as_str()),
        });
    }
    out
}

fn type_label(raw: &Value, resolved: &Value) -> String {
    if let Some(name) = ref_name(raw) {
        return name.to_string();
    }
    match resolved.get("type").and_then(Value::as_str) {
        Some("array") => {
            let items = resolved.get("items").cloned().unwrap_or(Value::Null);
            format!("{}[]", type_label(&items, &items))
        }
        Some(kind) => match resolved.get("format").and_then(Value::as_str) {
            Some(format) => format!("{kind} ({format})"),
            None => kind.to_string(),
        },
        None => "object".to_string(),
    }
}

fn flag(member: &Member, key: &str) -> bool {
    [&member.raw, &member.resolved].iter().any(|v| v.get(key).and_then(Value::as_bool) == Some(true))
}

fn description(member: &Member) -> String {
    [&member.raw, &member.resolved]
        .iter()
        .find_map(|v| v.get("description").and_then(Value::as_str))
        .map(cell)
        .unwrap_or_default()
}

/// What the specification says about the `PUT` of a type: its name parameter and
/// its body.
fn put_contract(out: &mut String, resolver: &mut Resolver, put: &Operation) {
    let parameters: Vec<Value> = put
        .definition
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| resolver.resolve(&put.file, p).map(|(_, v)| v))
        .collect();

    let name_parameter = put.path.rsplit('/').next().and_then(|s| s.strip_prefix('{')?.strip_suffix('}'));
    let named = parameters
        .iter()
        .find(|p| p.get("name").and_then(Value::as_str) == name_parameter && name_parameter.is_some());
    if let Some(parameter) = named {
        let mut rules = Vec::new();
        let bound = |key: &str| parameter.get(key).and_then(Value::as_u64);
        match (bound("minLength"), bound("maxLength")) {
            (Some(min), Some(max)) => rules.push(format!("{min}–{max} characters")),
            (Some(min), None) => rules.push(format!("at least {min} characters")),
            (None, Some(max)) => rules.push(format!("at most {max} characters")),
            (None, None) => {}
        }
        if let Some(pattern) = parameter.get("pattern").and_then(Value::as_str) {
            rules.push(format!("pattern `{pattern}`"));
        }
        if !rules.is_empty() {
            let _ = writeln!(out, "**Name** (`{}`): {}.\n", name_parameter.unwrap_or(""), rules.join(", "));
        }
    }

    let Some(body) = parameters.iter().find(|p| p.get("in").and_then(Value::as_str) == Some("body")) else {
        return;
    };
    let schema = body.get("schema").cloned().unwrap_or(Value::Null);
    let top = members(resolver, &put.file, &schema, 0);
    if top.is_empty() {
        return;
    }
    let envelope: Vec<String> = top
        .iter()
        .filter(|m| m.name != "properties" && !flag(m, "readOnly"))
        .map(|m| format!("`{}`{}", m.name, if m.required { " (required)" } else { "" }))
        .collect();
    let schema_name = ref_name(&schema).unwrap_or("inline schema");
    let _ = write!(out, "**Body** (`{schema_name}`, api-version {})", put.version);
    match envelope.is_empty() {
        true => out.push_str(".\n\n"),
        false => {
            let _ = writeln!(out, ": {}.\n", envelope.join(", "));
        }
    }

    let Some(properties) = top.iter().find(|m| m.name == "properties") else {
        return;
    };
    let rows = members(resolver, &properties.file, &properties.resolved, 0);
    if rows.is_empty() {
        return;
    }
    if properties.required {
        out.push_str("`properties` is required.\n\n");
    }
    out.push_str("| Property | Type | Required | Read-only | Description |\n|---|---|---|---|---|\n");
    for row in &rows {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} | {} |",
            row.name,
            cell(&type_label(&row.raw, &row.resolved)),
            if row.required { "yes" } else { "" },
            if flag(row, "readOnly") { "yes" } else { "" },
            description(row),
        );
    }
    out.push('\n');
}

fn operations_table<'a>(out: &mut String, operations: impl Iterator<Item = &'a Operation>) {
    out.push_str("| Operation | Method | Path | Routed as |\n|---|---|---|---|\n");
    for operation in operations {
        let routed = match &operation.kind {
            Kind::Other => "not routed yet".to_string(),
            other => kind_literal(other).replace('"', "`"),
        };
        let _ = writeln!(out, "| `{}` | {} | `{}` | {} |", operation.id, operation.method, operation.path, routed);
    }
    out.push('\n');
}

fn contract(provider: &Provider, resolver: &mut Resolver) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {} — modeled contract\n", provider.namespace);
    out.push_str("Generated by `emulator-scaffold` from the provider's OpenAPI specification. Do not\nedit: rerun the generator. This is what the specification says, not what the\nemulator implements — run `azure-emulator coverage` for that.\n\n");
    for kind in provider.types.values() {
        type_contract(&mut out, resolver, kind);
    }
    if !provider.operations.is_empty() {
        out.push_str("## Provider-level operations\n\n");
        operations_table(&mut out, provider.operations.values());
    }
    out
}

fn type_contract(out: &mut String, resolver: &mut Resolver, kind: &ResourceType) {
    let _ = writeln!(out, "## {}\n", kind.name);
    let (first, last) = (kind.versions.first(), kind.versions.last());
    if let (Some(first), Some(last)) = (first, last) {
        let _ = writeln!(out, "{} api-versions, `{first}` to `{last}`.\n", kind.versions.len());
    }
    operations_table(out, kind.operations.values());
    if let Some(put) = kind.operations.values().find(|o| o.kind == Kind::Put) {
        put_contract(out, resolver, put);
    }
}

/// The worked examples the specification gives for creating each type, for the
/// spec-example conformance test: path parameters, request body, the statuses
/// the operation answers with, and the example response.
fn spec_examples(provider: &Provider, resolver: &mut Resolver) -> String {
    let mut out = Vec::new();
    for kind in provider.types.values() {
        let Some(put) = kind.operations.values().find(|o| o.kind == Kind::Put) else {
            continue;
        };
        let parameters: Vec<Value> = put
            .definition
            .get("parameters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| resolver.resolve(&put.file, p).map(|(_, v)| v))
            .collect();
        let body_name = parameters
            .iter()
            .find(|p| p.get("in").and_then(Value::as_str) == Some("body"))
            .and_then(|p| p.get("name").and_then(Value::as_str))
            .map(str::to_string);
        let statuses: Vec<u16> = put
            .definition
            .get("responses")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter_map(|(code, _)| code.parse().ok())
            .collect();
        let examples = put.definition.get("x-ms-examples").and_then(Value::as_object);
        for (title, reference) in examples.into_iter().flatten() {
            let Some((_, example)) = resolver.resolve(&put.file, reference) else {
                continue;
            };
            let given = example.get("parameters").cloned().unwrap_or(Value::Null);
            let mut path_parameters = serde_json::Map::new();
            for (name, value) in given.as_object().into_iter().flatten() {
                if Some(name) != body_name.as_ref() && value.is_string() {
                    path_parameters.insert(name.clone(), value.clone());
                }
            }
            let body = body_name.as_ref().and_then(|name| given.get(name)).cloned().unwrap_or(Value::Null);
            let response = example
                .get("responses")
                .and_then(Value::as_object)
                .and_then(|responses| responses.iter().find(|(code, _)| code.starts_with('2')))
                .and_then(|(_, response)| response.get("body"))
                .cloned()
                .unwrap_or(Value::Null);
            out.push(serde_json::json!({
                "type": kind.name,
                "operationId": put.id,
                "title": title,
                "path": put.path,
                "parameters": path_parameters,
                "body": body,
                "statuses": statuses,
                "response": response,
            }));
        }
    }
    let mut text = serde_json::to_string_pretty(&Value::Array(out)).unwrap_or_default();
    text.push('\n');
    text
}

/// Inserts `line` into `file` immediately before or after the first line equal to
/// `anchor`, unless the file already contains `line`.
fn insert_line(file: &Path, anchor: &str, line: &str, before: bool) -> Result<()> {
    let text = std::fs::read_to_string(file)?;
    if text.lines().any(|l| l == line) {
        return Ok(());
    }
    let mut out = String::with_capacity(text.len() + line.len() + 1);
    let mut inserted = false;
    for existing in text.lines() {
        let here = !inserted && existing == anchor;
        if here && before {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(existing);
        out.push('\n');
        if here && !before {
            out.push_str(line);
            out.push('\n');
        }
        inserted |= here;
    }
    if !inserted {
        return Err(format!("{}: anchor line {anchor:?} not found; add {line:?} by hand", file.display()).into());
    }
    std::fs::write(file, out)?;
    Ok(())
}

/// Anchor in `core/router/src/lib.rs` above which new services are registered.
const ROUTER_ANCHOR: &str = "        // emulator-scaffold: new services are registered above this line.";

/// Writes the generated files. Returns whether the crate was newly created.
pub fn write_crate(repo: &Path, crate_dir: &Path, service: &str, provider: &Provider) -> Result<bool> {
    let created = !crate_dir.join("Cargo.toml").exists();
    std::fs::create_dir_all(crate_dir.join("src"))?;
    std::fs::write(crate_dir.join("src/scaffold.rs"), scaffold_source(provider))?;
    let mut resolver = Resolver::default();
    std::fs::write(crate_dir.join("CONTRACT.md"), contract(provider, &mut resolver))?;
    std::fs::write(crate_dir.join("spec-examples.json"), spec_examples(provider, &mut resolver))?;

    if created {
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"service-{service}\"\nversion.workspace = true\nedition.workspace = true\n\n[dependencies]\nemulator-core = {{ workspace = true }}\n\n[lints]\nworkspace = true\n"
            ),
        )?;
        std::fs::write(
            crate_dir.join("src/lib.rs"),
            format!(
                "//! `{namespace}` resource provider.\n//!\n//! Every resource type in the provider's API specification is mounted from\n//! [`scaffold::PROVIDER`] and answers `501` until it is implemented. To implement a\n//! type, write a `ResourceTypeHandler` for it and register it here, before the\n//! scaffold. `CONTRACT.md` describes what the specification says about each type.\n\npub mod scaffold;\n\nuse emulator_core::arm::ArmRegistry;\n\n/// Mounts the provider on Resource Manager.\npub fn register(registry: &mut ArmRegistry) {{\n    registry.register_scaffold(&scaffold::PROVIDER);\n}}\n",
                namespace = provider.namespace
            ),
        )?;
        let dependency = format!("service-{service} = {{ path = \"services/{service}\" }}");
        let root = repo.join("Cargo.toml");
        insert_line(&root, "    \"tools/scaffold\",", &format!("    \"services/{service}\","), true)?;
        insert_line(&root, "emulator-core = { path = \"core/emulator-core\" }", &dependency, false)?;
        insert_line(
            &repo.join("core/router/Cargo.toml"),
            "emulator-core = { workspace = true }",
            &format!("service-{service} = {{ workspace = true }}"),
            false,
        )?;
        insert_line(
            &repo.join("core/router/src/lib.rs"),
            ROUTER_ANCHOR,
            &format!("        service_{service}::register(&mut registry);"),
            true,
        )?;
    } else {
        let lib = std::fs::read_to_string(crate_dir.join("src/lib.rs")).unwrap_or_default();
        if !lib.contains("scaffold::PROVIDER") {
            eprintln!(
                "note: services/{service}/src/lib.rs does not mount the scaffold yet; add `pub mod scaffold;` and call `registry.register_scaffold(&scaffold::PROVIDER)` at the end of `register`"
            );
        }
    }
    Ok(created)
}
