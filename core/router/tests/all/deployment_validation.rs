//! Deployments beyond the happy path: parameter validation, expression syntax,
//! template structure, child and nested resources, the deployment routes
//! (cancel, status, operations, existence), and what-if property changes.
//!
//! Expected codes follow the Resource Manager deployment reference and the
//! template syntax documentation.

use http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::support::{Client, Reply, SUB};

const API: &str = "api-version=2022-09-01";

fn path(group: &str, name: &str) -> String {
    format!("{SUB}/resourceGroups/{group}/providers/Microsoft.Resources/deployments/{name}")
}

fn template(parameters: Value, resources: Value, outputs: Value) -> Value {
    json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "parameters": parameters,
        "resources": resources,
        "outputs": outputs,
    })
}

async fn put(client: &Client, group: &str, name: &str, body: Value) -> Reply {
    client.arm(Method::PUT, &format!("{}?{API}", path(group, name)), Some(body)).await
}

async fn deploy(client: &Client, group: &str, name: &str, template: Value, parameters: Value) -> Reply {
    put(client, group, name, json!({"properties": {"mode": "Incremental", "template": template, "parameters": parameters}})).await
}

fn succeeded(reply: &Reply) -> Value {
    assert!(reply.status.is_success(), "{}", reply.text());
    let body = reply.json();
    assert_eq!(body["properties"]["provisioningState"], "Succeeded", "{}", reply.text());
    body
}

fn refused_with(reply: &Reply, code: &str, fragment: &str) {
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
    assert_eq!(reply.error_code(), code, "{}", reply.text());
    let message = reply.json()["error"]["message"].as_str().unwrap_or("").to_string();
    assert!(message.contains(fragment), "expected '{fragment}' in: {message}");
}

/// Refused before anything runs, or run and failed: either way with `fragment`
/// in the error. (Some errors need evaluation to find.)
fn fails_with(reply: &Reply, fragment: &str) {
    let text = reply.text();
    let refused = reply.status == StatusCode::BAD_REQUEST && reply.error_code() == "InvalidTemplate";
    let failed = reply.status.is_success() && reply.json()["properties"]["provisioningState"] == "Failed";
    assert!(refused || failed, "{text}");
    assert!(text.contains(fragment), "expected '{fragment}' in: {text}");
}

fn identity(name: &str) -> Value {
    json!({"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": name, "location": "eastus"})
}

fn parameter_definitions() -> Value {
    json!({
        "count": {"type": "int", "minValue": 1, "maxValue": 5, "defaultValue": 2},
        "letters": {"type": "array", "minLength": 1, "maxLength": 3, "allowedValues": ["a", "b", "c"], "defaultValue": ["a"]},
        "label": {"type": "string", "minLength": 2, "maxLength": 4, "defaultValue": "[concat('x', 'y')]"},
        "flag": {"type": "bool", "defaultValue": false},
        "settings": {"type": "object", "defaultValue": {}},
        "optional": {"type": "string", "nullable": true},
    })
}

#[tokio::test]
async fn parameter_values_are_validated_against_their_definitions() {
    let client = Client::new();
    client.create_group("params").await;
    let empty = template(parameter_definitions(), json!([]), json!({}));
    let cases = [
        (json!({"count": {"value": 0}}), "greater than or equal to '1'"),
        (json!({"count": {"value": 6}}), "less than or equal to '5'"),
        (json!({"count": {"value": "3"}}), "Expected 'Int'. Actual 'String'"),
        (json!({"letters": {"value": []}}), "Length of the value should be greater than or equal to '1'"),
        (json!({"letters": {"value": ["a", "b", "c", "a"]}}), "Length of the value should be less than or equal to '3'"),
        (json!({"letters": {"value": ["a", "d"]}}), "The provided value 'd'"),
        (json!({"label": {"value": "x"}}), "greater than or equal to '2'"),
        (json!({"label": {"value": "xxxxx"}}), "less than or equal to '4'"),
        (json!({"flag": {"value": "true"}}), "Expected 'Bool'. Actual 'String'"),
        (json!({"settings": {"value": []}}), "Expected 'Object'. Actual 'Array'"),
        (json!({"letters": {"value": {"a": 1}}}), "Expected 'Array'. Actual 'Object'"),
    ];
    for (parameters, fragment) in cases {
        let reply = deploy(&client, "params", "d", empty.clone(), parameters.clone()).await;
        refused_with(&reply, "InvalidTemplate", fragment);
    }
    // A parameter entry with neither a value nor a reference is malformed.
    let reply = deploy(&client, "params", "d", empty.clone(), json!({"count": {}})).await;
    refused_with(&reply, "InvalidRequestContent", "is null");
}

#[tokio::test]
async fn defaults_nullable_parameters_and_parameter_files() {
    let client = Client::new();
    client.create_group("defaults").await;
    let outputs = json!({
        "count": {"type": "int", "value": "[parameters('count')]"},
        "label": {"type": "string", "value": "[parameters('label')]"},
        "missing": {"type": "bool", "value": "[equals(parameters('optional'), null())]"},
        "letters": {"type": "array", "value": "[parameters('letters')]"},
    });
    let body = template(parameter_definitions(), json!([]), outputs);
    let deployment = succeeded(&deploy(&client, "defaults", "d", body.clone(), json!({})).await);
    let out = &deployment["properties"]["outputs"];
    assert_eq!(
        (&out["count"]["value"], &out["label"]["value"], &out["missing"]["value"], &out["letters"]["value"]),
        (&json!(2), &json!("xy"), &json!(true), &json!(["a"]))
    );
    assert_eq!(out["count"]["type"], "Int");

    // A whole parameters file is accepted in place of the parameters object.
    let file = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentParameters.json#",
        "contentVersion": "1.0.0.0",
        "parameters": {"count": {"value": 5}, "letters": {"value": ["b", "c"]}},
    });
    let deployment = succeeded(&deploy(&client, "defaults", "file", body, file).await);
    assert_eq!(deployment["properties"]["outputs"]["count"]["value"], 5);
    assert_eq!(deployment["properties"]["outputs"]["letters"]["value"], json!(["b", "c"]));
}

#[tokio::test]
async fn expression_syntax_errors_are_refused() {
    let client = Client::new();
    client.create_group("syntax").await;
    let cases = [
        "[concat('a') 'b']",
        "['unterminated]",
        "[concat('a'; 'b')]",
        "[concat('a').]",
        "[resourceGroup]",
        "[concat('a',]",
        "[concat('a'?]",
        "[]",
    ];
    for expression in cases {
        let body = template(json!({}), json!([]), json!({"o": {"type": "string", "value": expression}}));
        let reply = deploy(&client, "syntax", "d", body, json!({})).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{expression}: {}", reply.text());
        assert_eq!(reply.error_code(), "InvalidTemplate", "{expression}: {}", reply.text());
        assert!(reply.text().contains("outputs.o"), "the error names where: {}", reply.text());
    }
    // Nothing was recorded.
    let reply = client.arm(Method::GET, &format!("{}?{API}", path("syntax", "d")), None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn expression_forms_that_evaluate() {
    let client = Client::new();
    client.create_group("forms").await;
    let parameters = json!({"settings": {"type": "object", "defaultValue": {"a": {"b": 1}, "list": [10, 20]}}});
    let cases = [
        ("[parameters('settings')?.missing]", Value::Null),
        ("[parameters('settings')?.a.b]", json!(1)),
        ("[parameters('settings')?['a']?['b']]", json!(1)),
        ("[parameters('settings')?['nope']?['b']]", Value::Null),
        ("[parameters('settings').list[1]]", json!(20)),
        ("[concat('it''s', ' ok')]", json!("it's ok")),
        ("[[not an expression]", json!("[not an expression]")),
        ("plain text", json!("plain text")),
        ("[  add( 1 ,2 )  ]", json!(3)),
    ];
    let outputs: serde_json::Map<String, Value> = cases
        .iter()
        .enumerate()
        .map(|(i, (expression, _))| (format!("o{i}"), json!({"type": "object", "value": expression})))
        .collect();
    let body = template(parameters, json!([]), Value::Object(outputs));
    let deployment = succeeded(&deploy(&client, "forms", "d", body, json!({})).await);
    let mut wrong = Vec::new();
    for (i, (expression, expected)) in cases.iter().enumerate() {
        let got = &deployment["properties"]["outputs"][format!("o{i}")]["value"];
        if got != expected {
            wrong.push(format!("{expression}: got {got}, expected {expected}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[tokio::test]
async fn variables_that_refer_to_themselves_or_nothing_are_refused() {
    let client = Client::new();
    client.create_group("vars").await;
    for (variables, fragment) in [
        (json!({"v": "[variables('v')]"}), "refers to itself"),
        (json!({"a": "[variables('b')]", "b": "[variables('a')]"}), "refers to itself"),
        (json!({"v": "[variables('missing')]"}), "is not found"),
    ] {
        let mut body = template(json!({}), json!([]), json!({"o": {"type": "string", "value": "[variables('v')]"}}));
        body["variables"] = variables;
        if body["variables"].get("v").is_none() {
            body["outputs"]["o"]["value"] = json!("[variables('a')]");
        }
        fails_with(&deploy(&client, "vars", "d", body, json!({})).await, fragment);
    }
}

#[tokio::test]
async fn malformed_templates_are_refused() {
    let client = Client::new();
    client.create_group("shape").await;
    let with_resources = |resources: Value| template(json!({}), resources, json!({}));
    let mut bad_condition = identity("a");
    bad_condition["condition"] = json!("[concat('y', 'es')]");
    let mut bad_depends = identity("a");
    bad_depends["dependsOn"] = json!("b");
    let mut numeric_name = identity("a");
    numeric_name["name"] = json!(5);
    let cases = [
        (json!([1, 2]), "must be a JSON object"),
        (json!("template"), "must be a JSON object"),
        (with_resources(json!(5)), "must be an array or an object"),
        (with_resources(json!([5])), "a template resource must be a JSON object"),
        (with_resources(json!([numeric_name])), "must be a string"),
        (with_resources(json!([bad_condition])), "must be a boolean"),
        (with_resources(json!([bad_depends])), "must be an array"),
        (with_resources(json!([identity("a"), {"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "b", "location": "[concat('east', 'us']"}])), "resources[1].location"),
    ];
    for (body, fragment) in cases {
        refused_with(&deploy(&client, "shape", "d", body, json!({})).await, "InvalidTemplate", fragment);
    }
    // Nothing ran: the valid resource beside the broken one was not created.
    let a = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/shape/providers/Microsoft.ManagedIdentity/userAssignedIdentities/a?api-version=2023-01-31"), None)
        .await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);

    // A reference to a resource the template does not declare.
    let body = with_resources(json!([{"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "a", "location": "[reference('nowhere').x]"}]));
    fails_with(&deploy(&client, "shape", "d", body, json!({})).await, "could not find template resource");
}

#[tokio::test]
async fn conditions_written_as_strings_and_child_resources() {
    let client = Client::new();
    client.create_group("children").await;
    let mut skipped = identity("skipped");
    skipped["condition"] = json!("false");
    let mut kept = identity("kept");
    kept["condition"] = json!("True");
    let namespace = json!({
        "type": "Microsoft.ServiceBus/namespaces",
        "apiVersion": "2022-10-01-preview",
        "name": "children-ns-1",
        "location": "eastus",
        "sku": {"name": "Standard"},
        "resources": [{
            "type": "queues",
            "apiVersion": "2022-10-01-preview",
            "name": "jobs",
            "dependsOn": ["[resourceId('Microsoft.ServiceBus/namespaces', 'children-ns-1')]"],
            "properties": {"maxDeliveryCount": 3},
        }],
    });
    let body = template(json!({}), json!([skipped, kept, namespace]), json!({}));
    succeeded(&deploy(&client, "children", "d", body, json!({})).await);

    let base = format!("{SUB}/resourceGroups/children/providers");
    let identity_path = |name: &str| format!("{base}/Microsoft.ManagedIdentity/userAssignedIdentities/{name}?api-version=2023-01-31");
    assert_eq!(client.arm(Method::GET, &identity_path("skipped"), None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(client.arm(Method::GET, &identity_path("kept"), None).await.status, StatusCode::OK);
    let queue = client
        .arm(Method::GET, &format!("{base}/Microsoft.ServiceBus/namespaces/children-ns-1/queues/jobs?api-version=2022-10-01-preview"), None)
        .await;
    assert_eq!(queue.status, StatusCode::OK, "{}", queue.text());
    assert_eq!(queue.json()["properties"]["maxDeliveryCount"], 3);
}

#[tokio::test]
async fn nested_deployments_in_outer_and_inner_scope() {
    let client = Client::new();
    client.create_group("nested").await;
    let nested = |name: &str, scope: Option<&str>, inner: Value, parameters: Value| {
        let mut properties = json!({"mode": "Incremental", "template": inner, "parameters": parameters});
        if let Some(scope) = scope {
            properties["expressionEvaluationOptions"] = json!({"scope": scope});
        }
        json!({"type": "Microsoft.Resources/deployments", "apiVersion": "2022-09-01", "name": name, "properties": properties})
    };
    // Outer scope: the nested template reads the parent's parameters and variables.
    let outer = nested(
        "outer",
        None,
        json!({
            "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
            "contentVersion": "1.0.0.0",
            "resources": [{"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31",
                           "name": "[concat(parameters('prefix'), variables('suffix'))]", "location": "eastus"}],
        }),
        json!({}),
    );
    // Inner scope: it has its own parameters and outputs.
    let inner = nested(
        "inner",
        Some("inner"),
        template(
            json!({"who": {"type": "string"}}),
            json!([]),
            json!({"greeting": {"type": "string", "value": "[concat('hello ', parameters('who'))]"}}),
        ),
        json!({"who": {"value": "[parameters('prefix')]"}}),
    );
    let mut body = template(
        json!({"prefix": {"type": "string", "defaultValue": "id"}}),
        json!([outer, inner]),
        json!({"greeting": {"type": "string", "value": "[reference('inner').outputs.greeting.value]"}}),
    );
    body["variables"] = json!({"suffix": "-made-by-outer"});
    let deployment = succeeded(&deploy(&client, "nested", "parent", body, json!({})).await);
    assert_eq!(deployment["properties"]["outputs"]["greeting"]["value"], "hello id");
    let made = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/nested/providers/Microsoft.ManagedIdentity/userAssignedIdentities/id-made-by-outer?api-version=2023-01-31"), None)
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.text());
    // The nested deployments are deployments of their own.
    succeeded(&client.arm(Method::GET, &format!("{}?{API}", path("nested", "inner")), None).await);

    // A failing nested deployment fails its parent with the nested error.
    let failing = nested(
        "failing",
        Some("inner"),
        template(json!({}), json!([{"type": "Microsoft.Unknown/widgets", "apiVersion": "2020-01-01", "name": "w", "location": "eastus"}]), json!({})),
        json!({}),
    );
    let reply = deploy(&client, "nested", "broken", template(json!({}), json!([failing]), json!({})), json!({})).await;
    let body = reply.json();
    assert_eq!(body["properties"]["provisioningState"], "Failed", "{}", reply.text());
    assert_eq!(body["properties"]["error"]["details"][0]["code"], "DeploymentFailed");
}

#[tokio::test]
async fn deployment_request_refusals() {
    let client = Client::new();
    client.create_group("requests").await;
    let empty = template(json!({}), json!([]), json!({}));
    let cases = [
        ("bad*name", json!({"properties": {"mode": "Incremental", "template": empty}}), "InvalidDeploymentName"),
        (&"x".repeat(65), json!({"properties": {"mode": "Incremental", "template": empty}}), "InvalidDeploymentName"),
        ("d", json!({"properties": {"mode": "Partial", "template": empty}}), "InvalidRequestContent"),
        ("d", json!({"properties": {"mode": "Incremental"}}), "InvalidDeployment"),
        ("d", json!({}), "InvalidRequestContent"),
        ("d", json!([]), "InvalidRequestContent"),
    ];
    for (name, body, code) in cases {
        let reply = put(&client, "requests", name, body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{name}: {}", reply.text());
        assert_eq!(reply.error_code(), code, "{name}: {}", reply.text());
    }
    for link in ["templateLink", "parametersLink"] {
        let mut body = json!({"properties": {"mode": "Incremental", "template": empty}});
        body["properties"][link] = json!({"uri": "https://example.com/t.json"});
        assert_eq!(put(&client, "requests", "d", body).await.status, StatusCode::NOT_IMPLEMENTED, "{link}");
    }

    // At subscription scope: a location is required and Complete mode is refused.
    let subscription = format!("{SUB}/providers/Microsoft.Resources/deployments/sub-d?{API}");
    let reply = client.arm(Method::PUT, &subscription, Some(json!({"properties": {"mode": "Incremental", "template": empty}}))).await;
    assert_eq!(reply.error_code(), "LocationRequired", "{}", reply.text());
    let reply = client
        .arm(Method::PUT, &subscription, Some(json!({"location": "eastus", "properties": {"mode": "Complete", "template": empty}})))
        .await;
    assert_eq!(reply.error_code(), "InvalidDeploymentMode", "{}", reply.text());

    // Unknown group.
    let reply = put(&client, "no-such-group", "d", json!({"properties": {"mode": "Incremental", "template": empty}})).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deployment_routes() {
    let client = Client::new();
    client.create_group("routes").await;
    let body = template(json!({}), json!([identity("one")]), json!({}));
    let deployment = succeeded(&deploy(&client, "routes", "d", body.clone(), json!({})).await);
    let base = path("routes", "d");

    assert_eq!(client.arm(Method::HEAD, &format!("{base}?{API}"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(client.arm(Method::HEAD, &format!("{}?{API}", path("routes", "missing")), None).await.status, StatusCode::NOT_FOUND);
    let missing = client.arm(Method::GET, &format!("{}?{API}", path("routes", "missing")), None).await;
    assert_eq!(missing.error_code(), "DeploymentNotFound");

    // A finished deployment cannot be cancelled.
    let reply = client.arm(Method::POST, &format!("{base}/cancel?{API}"), None).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.error_code(), "DeploymentCannotBeCancelled");

    let status = client.arm(Method::GET, &format!("{base}/operationStatuses/anything?{API}"), None).await;
    assert_eq!(status.json()["status"], "Succeeded", "{}", status.text());

    let operations = client.arm(Method::GET, &format!("{base}/operations?{API}"), None).await.json();
    let id = operations["value"][0]["operationId"].as_str().expect("an operation id").to_string();
    let one = client.arm(Method::GET, &format!("{base}/operations/{id}?{API}"), None).await;
    assert_eq!(one.status, StatusCode::OK);
    assert_eq!(one.json()["properties"]["targetResource"]["resourceName"], "one");
    let none = client.arm(Method::GET, &format!("{base}/operations/0000?{API}"), None).await;
    assert_eq!(none.error_code(), "DeploymentOperationNotFound");

    let exported = client.arm(Method::POST, &format!("{base}/exportTemplate?{API}"), None).await;
    assert_eq!(exported.json()["template"]["resources"][0]["name"], "one");

    let listed = client.arm(Method::GET, &format!("{SUB}/resourceGroups/routes/providers/Microsoft.Resources/deployments?{API}"), None).await;
    assert_eq!(listed.json()["value"][0]["name"], "d");
    assert_eq!(deployment["name"], "d");

    // Redeploying under the same name answers 200, not 201.
    let again = deploy(&client, "routes", "d", body, json!({})).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.text());

    assert_eq!(client.arm(Method::DELETE, &format!("{base}?{API}"), None).await.status, StatusCode::ACCEPTED);
    assert_eq!(client.arm(Method::DELETE, &format!("{base}?{API}"), None).await.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn what_if_reports_property_changes() {
    let client = Client::new();
    client.create_group("whatif").await;
    let identity_url = format!("{SUB}/resourceGroups/whatif/providers/Microsoft.ManagedIdentity/userAssignedIdentities/existing?api-version=2023-01-31");
    client.arm(Method::PUT, &identity_url, Some(json!({"location": "eastus", "tags": {"keep": "1", "change": "old"}}))).await;

    let mut changed = identity("existing");
    changed["location"] = json!("EastUS");
    changed["tags"] = json!({"keep": "1", "change": "new", "add": "x"});
    let body = template(json!({}), json!([changed, identity("fresh")]), json!({}));
    let reply = client
        .arm(
            Method::POST,
            &format!("{}/whatIf?{API}", path("whatif", "d")),
            Some(json!({"properties": {"mode": "Incremental", "template": body}})),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let changes = reply.json()["properties"]["changes"].as_array().cloned().unwrap();
    let existing = changes.iter().find(|c| c["resourceId"].as_str().unwrap().ends_with("/existing")).unwrap();
    assert_eq!(existing["changeType"], "Modify", "{existing}");
    let mut delta: Vec<String> = existing["delta"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| format!("{}:{}", d["path"].as_str().unwrap(), d["propertyChangeType"].as_str().unwrap()))
        .collect();
    delta.sort();
    // A location differing only in case is no change.
    assert_eq!(delta, ["tags.add:Create", "tags.change:Modify"]);
    let fresh = changes.iter().find(|c| c["resourceId"].as_str().unwrap().ends_with("/fresh")).unwrap();
    assert_eq!(fresh["changeType"], "Create", "{fresh}");
}
