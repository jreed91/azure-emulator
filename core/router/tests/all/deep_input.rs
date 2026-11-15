//! Deeply nested template input: the expression parser, the evaluator and
//! nested deployments refuse nesting past their limits with an error, instead
//! of recursing until the stack overflows and the whole process aborts.
//!
//! Each test runs on a thread with the 2 MiB stack a Tokio worker gets, in
//! whatever profile the tests are built in (a debug build uses the most stack),
//! so a limit too generous for that stack fails here as a crash.

use std::future::Future;

use http::{Method, StatusCode};
use serde_json::{json, Map, Value};

use crate::support::{Client, Reply, SUB};

const API: &str = "api-version=2021-04-01";

/// Runs `test` to completion on a fresh thread with a Tokio worker's stack.
fn on_worker_stack<F, Fut>(test: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(test())
        })
        .expect("thread")
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
}

async fn deploy(client: &Client, name: &str, template: Value) -> Reply {
    let body = json!({"properties": {"mode": "Incremental", "template": template}});
    let path = format!("{SUB}/resourcegroups/rg/providers/Microsoft.Resources/deployments/{name}?{API}");
    client.arm(Method::PUT, &path, Some(body)).await
}

fn template(variables: Value, functions: Value, outputs: Value) -> Value {
    json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "variables": variables,
        "functions": functions,
        "resources": [],
        "outputs": outputs,
    })
}

/// A template whose only output is `expression`.
fn output(kind: &str, expression: &str) -> Value {
    template(json!({}), json!([]), json!({"o": {"type": kind, "value": expression}}))
}

/// `[concat(concat(...concat('a')...))]`, `depth` calls deep.
fn nested_concat(depth: usize) -> String {
    format!("[{}'a'{}]", "concat(".repeat(depth), ")".repeat(depth))
}

/// `ns.f(x)` counts down to 0 by calling itself; `wrap` puts the recursive
/// call inside more expression (`{}` marks where).
fn countdown(wrap: &str) -> Value {
    let recurse = wrap.replace("{}", "ns.f(sub(parameters('x'), 1))");
    json!([{"namespace": "ns", "members": {"f": {
        "parameters": [{"name": "x", "type": "int"}],
        "output": {"type": "int", "value": format!("[if(equals(parameters('x'), 0), 0, {recurse})]")},
    }}}])
}

/// The start of a long reply, for assertion messages.
fn head(text: &str) -> &str {
    &text[..text.len().min(300)]
}

/// The output `o` of a deployment that must have succeeded.
fn succeeded(reply: &Reply) -> Value {
    assert!(reply.status.is_success(), "{}", head(&reply.text()));
    let deployment = reply.json();
    assert_eq!(deployment["properties"]["provisioningState"], "Succeeded", "{}", head(&reply.text()));
    deployment["properties"]["outputs"]["o"]["value"].clone()
}

/// The error of a deployment that ran and failed.
fn failed(reply: &Reply) -> String {
    assert!(reply.status.is_success(), "{}", head(&reply.text()));
    let deployment = reply.json();
    assert_eq!(deployment["properties"]["provisioningState"], "Failed", "{}", head(&reply.text()));
    deployment["properties"]["error"].to_string()
}

/// A template expression nested past the parser's limit is refused like any
/// other invalid expression (400 `InvalidTemplate`); the 2,000-deep one used to
/// overflow the stack and abort the process.
#[test]
fn over_deep_expressions_are_refused() {
    on_worker_stack(|| async {
        let client = Client::new();
        client.create_group("rg").await;
        let refused = |reply: &Reply| {
            assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", head(&reply.text()));
            assert_eq!(reply.error_code(), "InvalidTemplate");
            let message = reply.json()["error"]["message"].as_str().unwrap_or_default().to_string();
            assert!(message.contains("nested more than 64 levels deep"), "{}", head(&message));
        };
        for depth in [64, 2_000, 200_000] {
            refused(&deploy(&client, "deep", output("string", &nested_concat(depth))).await);
        }
        // Access chains nest too: each `.name` wraps what came before.
        let chain = format!("[createObject('a', 1){}]", ".a".repeat(100_000));
        refused(&deploy(&client, "chain", output("int", &chain)).await);
        // The emulator still serves requests.
        assert_eq!(succeeded(&deploy(&client, "after", output("string", &nested_concat(3))).await), "a");
    });
}

/// Nesting up to the limits still parses and evaluates, within the stack.
#[test]
fn nesting_up_to_the_limits_still_deploys() {
    on_worker_stack(|| async {
        let client = Client::new();
        client.create_group("rg").await;
        // 63 calls and the string inside them: 64 levels.
        assert_eq!(succeeded(&deploy(&client, "concat", output("string", &nested_concat(63))).await), "a");
        let ifs = format!("[{}'a'{}]", "if(true, ".repeat(63), ", 'b')".repeat(63));
        assert_eq!(succeeded(&deploy(&client, "if", output("string", &ifs)).await), "a");
        // Lambdas inside lambdas: `map` and `lambda` are a level each.
        let maps = format!("[{}createArray(1){}]", "map(createArray(1), lambda('x', ".repeat(31), "))".repeat(31));
        let mut expected = json!([1]);
        for _ in 0..31 {
            expected = json!([expected]);
        }
        assert_eq!(succeeded(&deploy(&client, "map", output("array", &maps)).await), expected);
        // A literal value about as deep as a request may carry, with an
        // expression inside.
        let mut deep = json!("[concat('a')]");
        let mut expected = json!("a");
        for _ in 0..120 {
            deep = json!([deep]);
            expected = json!([expected]);
        }
        let literal = template(json!({"v": deep}), json!([]), json!({"o": {"type": "array", "value": "[variables('v')]"}}));
        assert_eq!(succeeded(&deploy(&client, "literal", literal).await), expected);
        // Variables that use variables, and a function that calls itself.
        let mut variables = Map::new();
        variables.insert("v0".into(), json!("a"));
        for i in 1..40 {
            variables.insert(format!("v{i}"), json!(format!("[variables('v{}')]", i - 1)));
        }
        let chain = template(Value::Object(variables), json!([]), json!({"o": {"type": "string", "value": "[variables('v39')]"}}));
        assert_eq!(succeeded(&deploy(&client, "vars", chain).await), "a");
        let recursion = template(json!({}), countdown("{}"), json!({"o": {"type": "int", "value": "[ns.f(50)]"}}));
        assert_eq!(succeeded(&deploy(&client, "func", recursion).await), 0);
    });
}

/// Evaluation that goes too deep (a user-defined function that calls itself
/// without end, a long chain of variables) fails the deployment instead of
/// overflowing the stack, whatever each level puts on it.
#[test]
fn over_deep_evaluation_fails_the_deployment() {
    on_worker_stack(|| async {
        let client = Client::new();
        client.create_group("rg").await;
        for wrap in [
            "{}",
            "add({}, 0)",
            "first(map(createArray(1), lambda('y', first(map(createArray(1), lambda('z', {}))))))",
            "first(sort(createArray({}, 1), lambda('a', 'b', less(lambdaVariables('a'), lambdaVariables('b')))))",
        ] {
            let recursion = template(json!({}), countdown(wrap), json!({"o": {"type": "int", "value": "[ns.f(100000)]"}}));
            let error = failed(&deploy(&client, "func", recursion).await);
            assert!(error.contains("nested too deeply to evaluate"), "{wrap}: {}", head(&error));
        }
        let mut variables = Map::new();
        variables.insert("v0".into(), json!("a"));
        for i in 1..1_000 {
            variables.insert(format!("v{i}"), json!(format!("[variables('v{}')]", i - 1)));
        }
        let chain = template(Value::Object(variables), json!([]), json!({"o": {"type": "string", "value": "[variables('v999')]"}}));
        assert!(failed(&deploy(&client, "vars", chain).await).contains("nested too deeply to evaluate"));
    });
}

/// A value computed step by step can grow no deeper than a request may carry.
#[test]
fn computed_values_stay_shallow() {
    on_worker_stack(|| async {
        let client = Client::new();
        client.create_group("rg").await;
        let wrap = |steps: usize| {
            format!("[reduce(range(0, {steps}), createArray(), lambda('acc', 'i', createArray(lambdaVariables('acc'))))]")
        };
        let mut expected = json!([]);
        for _ in 0..100 {
            expected = json!([expected]);
        }
        assert_eq!(succeeded(&deploy(&client, "ok", output("array", &wrap(100))).await), expected);
        let error = failed(&deploy(&client, "deep", output("array", &wrap(5_000))).await);
        assert!(error.contains("nested more than 128 levels deep"), "{}", head(&error));
        // Variables built on cached variables, one level each.
        let mut variables = Map::new();
        variables.insert("v0".into(), json!([]));
        let mut outputs = Map::new();
        for i in 1..200 {
            variables.insert(format!("v{i}"), json!(format!("[createArray(variables('v{}'))]", i - 1)));
            outputs.insert(format!("o{i:03}"), json!({"type": "array", "value": format!("[variables('v{i}')]")}));
        }
        let reply = deploy(&client, "cached", template(Value::Object(variables), json!([]), Value::Object(outputs))).await;
        let error = failed(&reply);
        assert!(error.contains("nested more than 128 levels deep"), "{}", head(&error));
    });
}

/// Deployments nested in deployments spend the same budget: nested deeply,
/// with the innermost one evaluating as deep as it can, they fail cleanly; a
/// few levels deep, they still work.
#[test]
fn nested_deployments_share_the_budget() {
    on_worker_stack(|| async {
        let client = Client::new();
        client.create_group("rg").await;
        let nested = |levels: usize, count: usize| {
            let output = json!({"o": {"type": "int", "value": format!("[ns.f({count})]")}});
            let mut t = template(json!({}), countdown("{}"), output);
            for i in 0..levels {
                t = json!({
                    "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
                    "contentVersion": "1.0.0.0",
                    "resources": [{
                        "type": "Microsoft.Resources/deployments",
                        "apiVersion": "2021-04-01",
                        "name": format!("n{i}"),
                        "properties": {"mode": "Incremental", "expressionEvaluationOptions": {"scope": "inner"}, "template": t},
                    }],
                    "outputs": {"o": {"type": "int", "value": format!("[reference('n{i}').outputs.o.value]")}},
                });
            }
            t
        };
        assert_eq!(succeeded(&deploy(&client, "shallow", nested(4, 20)).await), 0);
        // 25 levels is about as deep as a request body's JSON can hold.
        for (levels, count) in [(10, 100_000), (25, 100_000)] {
            let error = failed(&deploy(&client, "deep", nested(levels, count)).await);
            assert!(error.contains("nested too deeply to evaluate"), "{levels}: {}", head(&error));
        }
    });
}
