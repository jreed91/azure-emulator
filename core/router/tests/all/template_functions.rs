//! Template functions, case by case: each expression is deployed as an
//! output and compared with the value Azure documents for it. The expected
//! values come from the template function reference's own examples where it
//! has them, not from this emulator.

use http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::support::{Client, SUB};

const API: &str = "api-version=2022-09-01";
const TENANT: &str = "11111111-1111-1111-1111-111111111111";

/// The output type a value is declared with.
fn output_type(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Number(_) => "int",
        Value::Bool(_) => "bool",
        Value::Array(_) => "array",
        _ => "object",
    }
}

/// Deploys every expression as an output in one deployment and checks each
/// against its expected value, naming the expressions that differ.
async fn check(group: &str, cases: &[(&str, Value)]) {
    let client = Client::new();
    client.create_group(group).await;
    let outputs: serde_json::Map<String, Value> = cases
        .iter()
        .enumerate()
        .map(|(i, (expression, expected))| (format!("o{i}"), json!({"type": output_type(expected), "value": format!("[{expression}]")})))
        .collect();
    let template = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "parameters": {"stamp": {"type": "string", "defaultValue": "[utcNow('yyyyMMdd')]"}},
        "resources": [],
        "outputs": outputs,
    });
    let body = json!({"properties": {"mode": "Incremental", "template": template}});
    let path = format!("{SUB}/resourceGroups/{group}/providers/Microsoft.Resources/deployments/cases?{API}");
    let reply = client.arm(Method::PUT, &path, Some(body)).await;
    assert!(reply.status.is_success(), "{}", reply.text());
    let deployment = reply.json();
    assert_eq!(deployment["properties"]["provisioningState"], "Succeeded", "{}", serde_json::to_string_pretty(&deployment).unwrap_or_default());
    let out = &deployment["properties"]["outputs"];
    let wrong: Vec<String> = cases
        .iter()
        .enumerate()
        .filter(|(i, (_, expected))| &out[format!("o{i}")]["value"] != expected)
        .map(|(i, (expression, expected))| format!("  {expression}\n    expected {expected}\n    got      {}", out[format!("o{i}")]["value"]))
        .collect();
    assert!(wrong.is_empty(), "{} of {} expressions differ:\n{}", wrong.len(), cases.len(), wrong.join("\n"));
}

/// Deploys an expression that must fail, and returns the deployment's error.
async fn fails(client: &Client, group: &str, name: &str, expression: &str) -> Value {
    let template = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "resources": [],
        "outputs": {"o": {"type": "object", "value": format!("[{expression}]")}},
    });
    let body = json!({"properties": {"mode": "Incremental", "template": template}});
    let path = format!("{SUB}/resourceGroups/{group}/providers/Microsoft.Resources/deployments/{name}?{API}");
    let reply = client.arm(Method::PUT, &path, Some(body)).await;
    if reply.status == StatusCode::BAD_REQUEST {
        return reply.json();
    }
    let deployment = reply.json();
    assert_eq!(deployment["properties"]["provisioningState"], "Failed", "{expression} should fail: {deployment}");
    deployment["properties"]["error"].clone()
}

#[tokio::test]
async fn string_functions() {
    check(
        "fn-strings",
        &[
            ("base64('one, two, three')", json!("b25lLCB0d28sIHRocmVl")),
            ("base64ToString('b25lLCB0d28sIHRocmVl')", json!("one, two, three")),
            ("base64ToJson(base64('{\"one\": \"a\", \"two\": \"b\"}'))", json!({"one": "a", "two": "b"})),
            ("concat('prefix', '-', 'suffix')", json!("prefix-suffix")),
            ("contains('OneTwoThree', 'e')", json!(true)),
            ("contains('OneTwoThree', 'z')", json!(false)),
            ("dataUri('Hello')", json!("data:text/plain;charset=utf8;base64,SGVsbG8=")),
            ("dataUriToString('data:;base64,SGVsbG8sIFdvcmxkIQ==')", json!("Hello, World!")),
            ("empty('')", json!(true)),
            ("empty('a')", json!(false)),
            ("startsWith('abcdef', 'ab')", json!(true)),
            ("startsWith('abcdef', 'AB')", json!(true)),
            ("startsWith('abcdef', 'e')", json!(false)),
            ("endsWith('abcdef', 'ef')", json!(true)),
            ("endsWith('abcdef', 'F')", json!(true)),
            ("endsWith('abcdef', 'e')", json!(false)),
            ("first('One Two Three')", json!("O")),
            ("last('One Two Three')", json!("e")),
            ("format('{0}, {1}. Formatted number: {2:N0}', 'Hello', 'User', 8175133)", json!("Hello, User. Formatted number: 8,175,133")),
            ("indexOf('test', 't')", json!(0)),
            ("indexOf('abcdef', 'CD')", json!(2)),
            ("indexOf('abcdef', 'z')", json!(-1)),
            ("lastIndexOf('test', 't')", json!(3)),
            ("lastIndexOf('abcdef', 'AB')", json!(0)),
            ("length('One Two Three')", json!(13)),
            ("padLeft('123', 10, '0')", json!("0000000123")),
            ("replace('123-123-1234', '-', '')", json!("1231231234")),
            ("skip('one two three', 4)", json!("two three")),
            ("take('one two three', 2)", json!("on")),
            ("split('one,two,three', ',')", json!(["one", "two", "three"])),
            ("string(createObject('valueA', 10, 'valueB', 'Example Text'))", json!("{\"valueA\":10,\"valueB\":\"Example Text\"}")),
            ("string(createArray('a', 'b', 'c'))", json!("[\"a\",\"b\",\"c\"]")),
            ("string(5)", json!("5")),
            ("substring('hello there', 6, 3)", json!("the")),
            ("toLower('One Two Three')", json!("one two three")),
            ("toUpper('One Two Three')", json!("ONE TWO THREE")),
            ("trim('    one two three   ')", json!("one two three")),
            ("uri('http://contoso.com/resources/', 'nested/azuredeploy.json')", json!("http://contoso.com/resources/nested/azuredeploy.json")),
            ("uriComponent('http://contoso.com/resources/nested/azuredeploy.json')", json!("http%3A%2F%2Fcontoso.com%2Fresources%2Fnested%2Fazuredeploy.json")),
            ("uriComponentToString('http%3A%2F%2Fcontoso.com%2Fresources%2Fnested%2Fazuredeploy.json')", json!("http://contoso.com/resources/nested/azuredeploy.json")),
            ("length(parameters('stamp'))", json!(8)),
            ("uniqueString('hello')", json!("htpi75jfihg4e")),
        ],
    )
    .await;
}

#[tokio::test]
async fn numeric_comparison_and_logical_functions() {
    check(
        "fn-numbers",
        &[
            ("add(5, 3)", json!(8)),
            ("sub(7, 3)", json!(4)),
            ("mul(5, 3)", json!(15)),
            ("div(8, 3)", json!(2)),
            ("mod(7, 3)", json!(1)),
            ("min(createArray(0, 3, 2, 5, 4))", json!(0)),
            ("max(createArray(0, 3, 2, 5, 4))", json!(5)),
            ("min(0, 3, 2, 5, 4)", json!(0)),
            ("max(0, 3, 2, 5, 4)", json!(5)),
            ("int('4')", json!(4)),
            ("range(1, 3)", json!([1, 2, 3])),
            ("range(5, 0)", json!([])),
            ("equals(1, 1)", json!(true)),
            ("equals('a', 'a')", json!(true)),
            ("equals(createArray('a', 'b'), createArray('a', 'b'))", json!(true)),
            ("equals(createObject('a', 1), createObject('a', 2))", json!(false)),
            ("greater(10, 5)", json!(true)),
            ("greater('A', 'a')", json!(false)),
            ("less('A', 'a')", json!(true)),
            ("greaterOrEquals(5, 5)", json!(true)),
            ("lessOrEquals(4, 5)", json!(true)),
            ("and(true, false)", json!(false)),
            ("and(true, true, true)", json!(true)),
            ("or(false, false)", json!(false)),
            ("or(false, true)", json!(true)),
            ("not(true)", json!(false)),
            ("bool('true')", json!(true)),
            ("bool(0)", json!(false)),
            ("bool(1)", json!(true)),
            ("if(equals(1, 1), 'yes', 'no')", json!("yes")),
            // The branch not taken is not evaluated.
            ("if(true, 'safe', div(1, 0))", json!("safe")),
            ("coalesce(null(), null(), 'third')", json!("third")),
            ("empty(null())", json!(true)),
        ],
    )
    .await;
}

#[tokio::test]
async fn array_and_object_functions() {
    check(
        "fn-collections",
        &[
            ("array('a')", json!(["a"])),
            ("array(createObject('a', 'b'))", json!([{"a": "b"}])),
            ("concat(createArray('1', '2'), createArray('3'))", json!(["1", "2", "3"])),
            ("contains(createArray('one', 'two'), 'two')", json!(true)),
            ("contains(createObject('one', 'a'), 'one')", json!(true)),
            ("contains(createObject('one', 'a'), 'two')", json!(false)),
            ("empty(createArray())", json!(true)),
            ("empty(createObject())", json!(true)),
            ("first(createArray('one', 'two'))", json!("one")),
            ("last(createArray('one', 'two'))", json!("two")),
            ("flatten(createArray(createArray('a'), createArray('b', 'c')))", json!(["a", "b", "c"])),
            ("indexOf(createArray('a', 'b', 'c'), 'b')", json!(1)),
            ("lastIndexOf(createArray('a', 'b', 'a'), 'a')", json!(2)),
            ("indexOf(createArray('a'), 'z')", json!(-1)),
            ("intersection(createArray('one', 'two', 'three'), createArray('two', 'three', 'four'))", json!(["two", "three"])),
            ("intersection(createObject('one', 'a', 'two', 'b'), createObject('one', 'a', 'two', 'z'))", json!({"one": "a"})),
            ("union(createArray('one', 'two', 'three'), createArray('three', 'four'))", json!(["one", "two", "three", "four"])),
            ("union(createObject('a', 1), createObject('b', 2))", json!({"a": 1, "b": 2})),
            ("length(createArray('a', 'b', 'c'))", json!(3)),
            ("length(createObject('a', 1, 'b', createObject('c', 2)))", json!(2)),
            ("skip(createArray('one', 'two', 'three'), 2)", json!(["three"])),
            ("take(createArray('one', 'two', 'three'), 2)", json!(["one", "two"])),
            ("take(createArray('one'), 5)", json!(["one"])),
            ("json('{\"a\": [1, true]}')", json!({"a": [1, true]})),
            ("objectKeys(createObject('only', 1))", json!(["only"])),
            ("shallowMerge(createArray(createObject('a', 1, 'b', createObject('x', 1)), createObject('b', 2)))", json!({"a": 1, "b": 2})),
            ("tryGet(createObject('a', createObject('b', 1)), 'a', 'b')", json!(1)),
            ("createObject('a', createArray(1, 2), 'b', createObject())", json!({"a": [1, 2], "b": {}})),
        ],
    )
    .await;
}

#[tokio::test]
async fn lambda_functions() {
    check(
        "fn-lambdas",
        &[
            ("filter(createArray(1, 2, 3, 4), lambda('x', greater(lambdaVariables('x'), 2)))", json!([3, 4])),
            ("map(createArray(1, 2), lambda('x', mul(lambdaVariables('x'), 10)))", json!([10, 20])),
            ("sort(createArray('b', 'c', 'a'), lambda('x', 'y', less(lambdaVariables('x'), lambdaVariables('y'))))", json!(["a", "b", "c"])),
            ("reduce(createArray(1, 2, 3), 10, lambda('acc', 'x', add(lambdaVariables('acc'), lambdaVariables('x'))))", json!(16)),
            ("groupBy(createArray('foo', 'bar', 'baz'), lambda('x', substring(lambdaVariables('x'), 0, 1)))", json!({"f": ["foo"], "b": ["bar", "baz"]})),
            ("mapValues(createObject('a', 1, 'b', 2), lambda('v', mul(lambdaVariables('v'), 2)))", json!({"a": 2, "b": 4})),
            (
                "toObject(createArray('a', 'b'), lambda('x', lambdaVariables('x')), lambda('x', toUpper(lambdaVariables('x'))))",
                json!({"a": "A", "b": "B"}),
            ),
            ("toObject(createArray('a', 'b'), lambda('x', lambdaVariables('x')))", json!({"a": "a", "b": "b"})),
            (
                "map(createArray('x', 'y'), lambda('v', createObject('name', lambdaVariables('v'))))",
                json!([{"name": "x"}, {"name": "y"}]),
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn date_functions() {
    check(
        "fn-dates",
        &[
            ("dateTimeToEpoch(dateTimeAdd('2020-04-07T14:53:14Z', 'P1D'))", json!(1586357594)),
            ("dateTimeToEpoch('2023-05-02T15:16:13Z')", json!(1683040573)),
            ("dateTimeToEpoch(dateTimeFromEpoch(1683040573))", json!(1683040573)),
            ("startsWith(dateTimeFromEpoch(1683040573), '2023-05-02T15:16:13')", json!(true)),
            ("dateTimeToEpoch(dateTimeAdd('2020-04-07T14:53:14Z', '-PT1H'))", json!(1586267594)),
        ],
    )
    .await;
}

#[tokio::test]
async fn deployment_and_resource_functions() {
    check(
        "fn-scope",
        &[
            ("subscription().subscriptionId", json!(SUB.trim_start_matches("/subscriptions/"))),
            ("subscription().id", json!(SUB)),
            ("tenant().tenantId", json!(TENANT)),
            ("resourceGroup().name", json!("fn-scope")),
            ("resourceGroup().id", json!(format!("{SUB}/resourceGroups/fn-scope"))),
            ("deployment().name", json!("cases")),
            ("resourceId('Microsoft.Storage/storageAccounts', 'acct')", json!(format!("{SUB}/resourceGroups/fn-scope/providers/Microsoft.Storage/storageAccounts/acct"))),
            ("resourceId('other', 'Microsoft.Storage/storageAccounts', 'acct')", json!(format!("{SUB}/resourceGroups/other/providers/Microsoft.Storage/storageAccounts/acct"))),
            // A resource group's id has no provider segment, as `resourceGroup().id` shows.
            ("subscriptionResourceId('Microsoft.Resources/resourceGroups', 'g')", json!(format!("{SUB}/resourceGroups/g"))),
            ("subscriptionResourceId('Microsoft.Authorization/roleDefinitions', 'r')", json!(format!("{SUB}/providers/Microsoft.Authorization/roleDefinitions/r"))),
            ("tenantResourceId('Microsoft.Authorization/policyDefinitions', 'p')", json!("/providers/Microsoft.Authorization/policyDefinitions/p")),
            (
                "extensionResourceId(resourceId('Microsoft.Storage/storageAccounts', 'acct'), 'Microsoft.Authorization/locks', 'lock')",
                json!(format!("{SUB}/resourceGroups/fn-scope/providers/Microsoft.Storage/storageAccounts/acct/providers/Microsoft.Authorization/locks/lock")),
            ),
            ("pickZones('Microsoft.Compute', 'virtualMachines', 'eastus')", json!([])),
        ],
    )
    .await;
}

#[tokio::test]
async fn invalid_calls_fail_the_deployment() {
    let client = Client::new();
    client.create_group("fn-errors").await;
    for (i, expression) in [
        "div(1, 0)",
        "substring('abc', 5, 1)",
        "notAFunction()",
        "int('not a number')",
        "parameters('missing')",
        "variables('missing')",
        "add(1)",
        "first(createObject())",
    ]
    .iter()
    .enumerate()
    {
        let error = fails(&client, "fn-errors", &format!("bad{i}"), expression).await;
        assert!(!error.is_null(), "{expression} should report an error");
    }
}
