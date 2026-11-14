//! Template deployments. The templates in `tests/templates/` were compiled from
//! the `.bicep` files beside them by the Bicep CLI, so they are what Bicep users
//! actually send.

use http::{Method, StatusCode};
use serde_json::{json, Value};

use crate::support::{Client, SUB};

const API: &str = "api-version=2022-09-01";

fn template(name: &str) -> Value {
    let path = format!("{}/tests/templates/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(&path).expect("template file")).expect("template JSON")
}

fn deployment_path(group: &str, name: &str) -> String {
    format!("{SUB}/resourceGroups/{group}/providers/Microsoft.Resources/deployments/{name}")
}

async fn deploy(client: &Client, group: &str, name: &str, template: Value, parameters: Value) -> Value {
    let body = json!({"properties": {"mode": "Incremental", "template": template, "parameters": parameters}});
    let reply = client.arm(Method::PUT, &format!("{}?{API}", deployment_path(group, name)), Some(body)).await;
    assert!(reply.status == StatusCode::CREATED || reply.status == StatusCode::OK, "{}", reply.text());
    reply.json()
}

fn outputs(deployment: &Value) -> &Value {
    assert_eq!(
        deployment["properties"]["provisioningState"], "Succeeded",
        "{}",
        serde_json::to_string_pretty(deployment).unwrap_or_default()
    );
    &deployment["properties"]["outputs"]
}

#[tokio::test]
async fn bicep_application_deploys_end_to_end() {
    let client = Client::new();
    client.create_group("app").await;
    let deployment = deploy(
        &client,
        "app",
        "main",
        template("app"),
        json!({"prefix": {"value": "demo"}, "adminSecret": {"value": "p@ss"}}),
    )
    .await;
    let out = outputs(&deployment);

    // `uniqueString(resourceGroup().id)` names the account as it would in Azure.
    let account = out["accountName"]["value"].as_str().expect("account name");
    assert!(account.starts_with("demo") && account.len() == 17, "{account}");
    let reply = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/app/providers/Microsoft.Storage/storageAccounts/{account}?api-version=2023-01-01"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let stored = reply.json();
    assert_eq!(out["blobEndpoint"]["value"], stored["properties"]["primaryEndpoints"]["blob"]);
    assert_eq!(stored["sku"]["name"], "Standard_LRS");

    // The copy loop made both containers; the conditional queue was skipped.
    let ids = out["containerIds"]["value"].as_array().expect("container ids");
    assert_eq!(ids.len(), 2);
    for id in ids {
        let reply = client.arm(Method::GET, &format!("{}?api-version=2023-01-01", id.as_str().unwrap()), None).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    }
    let reply = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/app/providers/Microsoft.Storage/storageAccounts/{account}/queueServices/default/queues/work?api-version=2023-01-01"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // The vault exists, with the secret `listKeys` filled in.
    let vault_uri = out["vaultUri"]["value"].as_str().expect("vault uri");
    assert!(vault_uri.starts_with("https://demo-kv-"), "{vault_uri}");
    let vault = vault_uri.trim_start_matches("https://").split('.').next().unwrap();
    let reply = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/app/providers/Microsoft.KeyVault/vaults/{vault}/secrets/storage-key?api-version=2023-07-01"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());

    // The module ran as a nested deployment and its output came back up.
    assert!(out["principalId"]["value"].as_str().is_some_and(|p| p.len() == 36));
    let reply = client.arm(Method::GET, &format!("{}?{API}", deployment_path("app", "identity")), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["properties"]["outputs"]["principalId"], out["principalId"]);

    // Secure parameters are reported without their value.
    let parameters = &deployment["properties"]["parameters"];
    assert_eq!(parameters["adminSecret"], json!({"type": "SecureString"}));
    assert_eq!(parameters["prefix"], json!({"type": "String", "value": "demo"}));
    assert_eq!(parameters["withQueue"]["value"], false);

    // Operations, listing, export.
    let reply = client.arm(Method::GET, &format!("{}/operations?{API}", deployment_path("app", "main")), None).await;
    let operations = reply.json()["value"].as_array().cloned().unwrap_or_default();
    assert!(operations.len() >= 7, "{}", reply.text());
    assert!(operations.iter().all(|o| o["properties"]["provisioningState"] == "Succeeded"));
    let reply = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/app/providers/Microsoft.Resources/deployments?{API}"), None)
        .await;
    let names: Vec<String> = reply.json()["value"].as_array().unwrap().iter().map(|d| d["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, ["identity", "main"]);
    let reply = client.arm(Method::POST, &format!("{}/exportTemplate?{API}", deployment_path("app", "main")), None).await;
    assert_eq!(reply.json()["template"], template("app"));

    // Deploying again updates in place.
    let body = json!({"properties": {"mode": "Incremental", "template": template("app"), "parameters": {"prefix": {"value": "demo"}, "adminSecret": {"value": "p@ss"}}}});
    let reply = client.arm(Method::PUT, &format!("{}?{API}", deployment_path("app", "main")), Some(body)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["properties"]["outputs"]["accountName"], out["accountName"]);

    // Deleting the group forgets its deployments.
    client.arm(Method::DELETE, &format!("{SUB}/resourceGroups/app?api-version=2021-04-01"), None).await;
    client.create_group("app").await;
    let reply = client.arm(Method::GET, &format!("{}?{API}", deployment_path("app", "main")), None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(reply.error_code(), "DeploymentNotFound");
}

#[tokio::test]
async fn symbolic_name_templates_with_functions_and_lambdas() {
    let client = Client::new();
    client.create_group("bus").await;
    let reply = client
        .arm(
            Method::PUT,
            &format!("{SUB}/resourceGroups/bus/providers/Microsoft.Storage/storageAccounts/existingacct?api-version=2023-01-01"),
            Some(json!({"location": "eastus", "sku": {"name": "Standard_LRS"}, "kind": "StorageV2"})),
        )
        .await;
    assert!(reply.status.is_success(), "{}", reply.text());

    let deployment = deploy(
        &client,
        "bus",
        "messaging",
        template("symbolic"),
        json!({"prefix": {"value": "shop"}, "accountName": {"value": "existingacct"}}),
    )
    .await;
    let out = outputs(&deployment);
    assert_eq!(out["queueNames"]["value"], json!(["orders", "invoices"]));
    assert_eq!(out["upper"]["value"], json!(["ORDERS", "INVOICES"]));
    assert_eq!(out["picked"]["value"], json!(["orders"]));
    assert!(out["blob"]["value"].as_str().unwrap().starts_with("https://existingacct.blob."));
    assert!(out["endpoint"]["value"].as_str().unwrap().contains("shop-bus-"));
    // A parameter typed by a user-defined type reports the underlying type.
    assert_eq!(deployment["properties"]["parameters"]["sku"], json!({"type": "String", "value": "Standard"}));

    // The copy index set each queue's delivery count.
    let namespace = deployment["properties"]["outputResources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["id"].as_str())
        .find(|id| id.ends_with("/queues/invoices"))
        .expect("invoices queue")
        .to_string();
    let reply = client.arm(Method::GET, &format!("{namespace}?api-version=2021-11-01"), None).await;
    assert_eq!(reply.json()["properties"]["maxDeliveryCount"], 6);

    // An `existing` resource that is not there fails the deployment.
    let deployment = deploy(
        &client,
        "bus",
        "missing",
        template("symbolic"),
        json!({"prefix": {"value": "shop2"}, "accountName": {"value": "nosuchacct"}}),
    )
    .await;
    assert_eq!(deployment["properties"]["provisioningState"], "Failed");
    assert_eq!(deployment["properties"]["error"]["code"], "DeploymentOutputEvaluationFailed");
}

#[tokio::test]
async fn subscription_deployment_creates_a_group_and_deploys_into_it() {
    let client = Client::new();
    let path = format!("{SUB}/providers/Microsoft.Resources/deployments/platform?{API}");
    let body = json!({"properties": {"mode": "Incremental", "template": template("subscription"), "parameters": {"groupName": {"value": "made-by-template"}}}});
    let reply = client.arm(Method::PUT, &path, Some(body.clone())).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.error_code(), "LocationRequired");

    let mut body = body;
    body["location"] = json!("westus");
    let reply = client.arm(Method::PUT, &path, Some(body)).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    let deployment = reply.json();
    let out = outputs(&deployment);
    assert_eq!(out["groupId"]["value"], format!("{SUB}/resourceGroups/made-by-template"));
    assert_eq!(deployment["location"], "westus");

    let reply = client.arm(Method::GET, &format!("{SUB}/resourceGroups/made-by-template?api-version=2021-04-01"), None).await;
    let group = reply.json();
    assert_eq!(group["location"], "westus");
    assert_eq!(group["tags"]["owner"], "templates");
    let reply = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/made-by-template/providers/Microsoft.ManagedIdentity/userAssignedIdentities/sub-made-id?api-version=2023-01-31"), None)
        .await;
    assert_eq!(reply.json()["properties"]["clientId"], out["clientId"]["value"]);
    let reply = client.arm(Method::GET, &format!("{SUB}/providers/Microsoft.Resources/deployments?{API}"), None).await;
    assert_eq!(reply.json()["value"][0]["name"], "platform");
}

fn plain_template(resources: Value) -> Value {
    json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "parameters": {"name": {"type": "string", "minLength": 3, "allowedValues": ["one", "two", "three"]}},
        "resources": resources,
    })
}

fn identity(name: &str) -> Value {
    json!({"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": name, "location": "eastus"})
}

#[tokio::test]
async fn invalid_templates_are_refused_before_anything_runs() {
    let client = Client::new();
    client.create_group("bad").await;
    let path = format!("{}?{API}", deployment_path("bad", "d"));
    let send = |template: Value, parameters: Value| {
        let client = &client;
        let path = path.clone();
        async move {
            let body = json!({"properties": {"mode": "Incremental", "template": template, "parameters": parameters}});
            client.arm(Method::PUT, &path, Some(body)).await
        }
    };
    let cases = [
        (plain_template(json!([])), json!({}), "is not provided"),
        (plain_template(json!([])), json!({"name": {"value": "four"}}), "allowed value"),
        (plain_template(json!([])), json!({"name": {"value": 3}}), "Expected 'String'"),
        (plain_template(json!([])), json!({"name": {"value": "one"}, "extra": {"value": 1}}), "not present in the original template"),
        (
            plain_template(json!([{"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "[concat('x']"}])),
            json!({"name": {"value": "one"}}),
            "concat('x'",
        ),
        (
            plain_template(json!([{"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "a", "dependsOn": ["b"]},
                                  {"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "b", "dependsOn": ["a"]}])),
            json!({"name": {"value": "one"}}),
            "Circular dependency",
        ),
        (
            plain_template(json!([{"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "a", "dependsOn": ["nope"]}])),
            json!({"name": {"value": "one"}}),
            "is not defined in the template",
        ),
        (
            plain_template(json!([{"type": "Microsoft.Storage/storageAccounts/blobServices", "apiVersion": "2023-01-01", "name": "onlyone"}])),
            json!({"name": {"value": "one"}}),
            "incorrect segment lengths",
        ),
    ];
    for (template, parameters, message) in cases {
        let reply = send(template, parameters).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
        assert_eq!(reply.error_code(), "InvalidTemplate");
        assert!(reply.json()["error"]["message"].as_str().unwrap().contains(message), "{}", reply.text());
    }
    let reply = client.arm(Method::GET, &path, None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // A body without a mode is malformed.
    let reply = client.arm(Method::PUT, &path, Some(json!({"properties": {"template": plain_template(json!([]))}}))).await;
    assert_eq!(reply.error_code(), "InvalidRequestContent");
}

#[tokio::test]
async fn a_failing_resource_fails_the_deployment_and_its_dependents() {
    let client = Client::new();
    client.create_group("partial").await;
    let template = plain_template(json!([
        identity("works"),
        {"type": "Microsoft.Unknown/widgets", "apiVersion": "2020-01-01", "name": "w", "location": "eastus"},
        {"type": "Microsoft.ManagedIdentity/userAssignedIdentities", "apiVersion": "2023-01-31", "name": "after", "location": "eastus",
         "dependsOn": ["[resourceId('Microsoft.Unknown/widgets', 'w')]"]},
    ]));
    let deployment = deploy(&client, "partial", "d", template, json!({"name": {"value": "one"}})).await;
    let properties = &deployment["properties"];
    assert_eq!(properties["provisioningState"], "Failed");
    assert_eq!(properties["error"]["code"], "DeploymentFailed");
    assert_eq!(properties["error"]["details"][0]["code"], "InvalidResourceNamespace");
    assert!(properties.get("outputs").is_none());

    let reply = client.arm(Method::GET, &format!("{}/operations?{API}", deployment_path("partial", "d")), None).await;
    let states: Vec<String> = reply.json()["value"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| format!("{}:{}", o["properties"]["targetResource"]["resourceName"].as_str().unwrap(), o["properties"]["provisioningState"].as_str().unwrap()))
        .collect();
    assert_eq!(states, ["works:Succeeded", "w:Failed"]);
    let reply = client
        .arm(Method::GET, &format!("{SUB}/resourceGroups/partial/providers/Microsoft.ManagedIdentity/userAssignedIdentities/after?api-version=2023-01-31"), None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn validate_what_if_complete_mode_and_template_hash() {
    let client = Client::new();
    client.create_group("modes").await;
    let template = plain_template(json!([identity("keep"), identity("[parameters('name')]")]));
    let body = |mode: &str| json!({"properties": {"mode": mode, "template": template, "parameters": {"name": {"value": "two"}}}});

    let reply = client.arm(Method::POST, &format!("{}/validate?{API}", deployment_path("modes", "d")), Some(body("Incremental"))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let validated = reply.json()["properties"]["validatedResources"].as_array().unwrap().len();
    assert_eq!(validated, 2);
    // Validation creates nothing.
    let reply = client.arm(Method::GET, &format!("{}?{API}", deployment_path("modes", "d")), None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);

    // An identity the template does not mention.
    let stray = format!("{SUB}/resourceGroups/modes/providers/Microsoft.ManagedIdentity/userAssignedIdentities/stray?api-version=2023-01-31");
    client.arm(Method::PUT, &stray, Some(json!({"location": "eastus"}))).await;
    client.arm(Method::PUT, &stray.replace("/stray?", "/keep?"), Some(json!({"location": "eastus"}))).await;

    let reply = client.arm(Method::POST, &format!("{}/whatIf?{API}", deployment_path("modes", "d")), Some(body("Complete"))).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let changes: Vec<String> = reply.json()["properties"]["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| format!("{}:{}", c["resourceId"].as_str().unwrap().rsplit('/').next().unwrap(), c["changeType"].as_str().unwrap()))
        .collect();
    assert_eq!(changes, ["keep:NoChange", "two:Create", "stray:Delete"]);

    let deployment = deploy_mode(&client, body("Complete")).await;
    assert_eq!(deployment["properties"]["mode"], "Complete");
    assert_eq!(deployment["properties"]["provisioningState"], "Succeeded", "{deployment}");
    let reply = client.arm(Method::GET, &stray, None).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let reply = client.arm(Method::GET, &stray.replace("/stray?", "/two?"), None).await;
    assert_eq!(reply.status, StatusCode::OK);

    let reply = client
        .arm(Method::POST, &format!("/providers/Microsoft.Resources/calculateTemplateHash?{API}"), Some(template.clone()))
        .await;
    assert_eq!(reply.json()["templateHash"], deployment["properties"]["templateHash"]);

    // Deleting a deployment leaves its resources alone.
    let reply = client.arm(Method::DELETE, &format!("{}?{API}", deployment_path("modes", "d")), None).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let reply = client.arm(Method::GET, &stray.replace("/stray?", "/two?"), None).await;
    assert_eq!(reply.status, StatusCode::OK);
}

async fn deploy_mode(client: &Client, body: Value) -> Value {
    let reply = client.arm(Method::PUT, &format!("{}?{API}", deployment_path("modes", "d")), Some(body)).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text());
    reply.json()
}

#[tokio::test]
async fn template_functions() {
    let client = Client::new();
    client.create_group("fns").await;
    let outputs_template = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "parameters": {"items": {"type": "array", "defaultValue": [3, 1, 2]}},
        "variables": {
            "base": {"a": {"x": 1}, "list": [1]},
            "copy": [{"name": "doubled", "count": 3, "input": "[mul(copyIndex('doubled'), 2)]"}],
        },
        "resources": [],
        "outputs": {
            "union": {"type": "object", "value": "[union(variables('base'), createObject('a', createObject('y', 2)))]"},
            "doubled": {"type": "array", "value": "[variables('doubled')]"},
            "sorted": {"type": "array", "value": "[sort(parameters('items'), lambda('a', 'b', less(lambdaVariables('a'), lambdaVariables('b'))))]"},
            "sum": {"type": "int", "value": "[reduce(parameters('items'), 0, lambda('acc', 'x', add(lambdaVariables('acc'), lambdaVariables('x'))))]"},
            "format": {"type": "string", "value": "[format('{0}-{1:D3}-{2}', 'a', 7, true)]"},
            "guid": {"type": "string", "value": "[guid('hello')]"},
            "rid": {"type": "string", "value": "[resourceId('Microsoft.Storage/storageAccounts/blobServices', 'acct', 'default')]"},
            "rg": {"type": "string", "value": "[resourceGroup().name]"},
            "escaped": {"type": "string", "value": "[[not an expression]"},
            "storageSuffix": {"type": "string", "value": "[environment().suffixes.storage]"},
            "safe": {"type": "object", "value": "[if(contains(variables('base'), 'missing'), variables('base').missing, createObject())]"},
            "tryGet": {"type": "string", "value": "[coalesce(tryGet(variables('base'), 'nope'), 'fallback')]"},
            "items": {"type": "array", "value": "[items(createObject('b', 2, 'a', 1))]"},
            "split": {"type": "array", "value": "[split('a,b;c', createArray(',', ';'))]"},
            "deploymentName": {"type": "string", "value": "[deployment().name]"},
            "conditional": {"type": "string", "condition": "[equals(1, 2)]", "value": "never"},
        },
    });
    let deployment = deploy(&client, "fns", "functions", outputs_template, json!({})).await;
    let out = outputs(&deployment);
    let value = |name: &str| out[name]["value"].clone();
    assert_eq!(value("union"), json!({"a": {"x": 1, "y": 2}, "list": [1]}));
    assert_eq!(value("doubled"), json!([0, 2, 4]));
    assert_eq!(value("sorted"), json!([1, 2, 3]));
    assert_eq!(value("sum"), json!(6));
    assert_eq!(value("format"), "a-007-True");
    assert_eq!(value("guid"), "520f8434-fe3a-5d99-888d-450a827486a1");
    assert_eq!(value("rid"), format!("{SUB}/resourceGroups/fns/providers/Microsoft.Storage/storageAccounts/acct/blobServices/default"));
    assert_eq!(value("rg"), "fns");
    assert_eq!(value("escaped"), "[not an expression]");
    assert_eq!(value("storageSuffix"), "core.azure.localhost:10443");
    assert_eq!(value("safe"), json!({}));
    assert_eq!(value("tryGet"), "fallback");
    assert_eq!(value("items"), json!([{"key": "a", "value": 1}, {"key": "b", "value": 2}]));
    assert_eq!(value("split"), json!(["a", "b", "c"]));
    assert_eq!(value("deploymentName"), "functions");
    assert!(out.get("conditional").is_none());
    assert_eq!(out["sum"]["type"], "Int");
}

#[tokio::test]
async fn bicep_role_assignment_on_a_resource() {
    let client = Client::new();
    client.create_group("roles").await;
    let deployment = deploy(&client, "roles", "main", template("roles"), json!({})).await;
    let out = outputs(&deployment);
    let id = out["assignmentId"]["value"].as_str().expect("assignment id");
    assert!(id.contains("/providers/Microsoft.Storage/storageAccounts/roles"), "{id}");
    let reply = client.arm(Method::GET, &format!("{id}?api-version=2022-04-01"), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let assignment = reply.json();
    assert_eq!(assignment["properties"]["principalId"], out["principalId"]["value"]);
    assert_eq!(assignment["properties"]["principalType"], "ServicePrincipal");
    assert!(assignment["properties"]["roleDefinitionId"].as_str().unwrap().ends_with("/2a2b9908-6ea1-4ae2-8e65-a410df84e7d1"));

    // Deploying again finds the same assignment, named by guid(), unchanged.
    let again = deploy(&client, "roles", "main", template("roles"), json!({})).await;
    assert_eq!(outputs(&again)["assignmentId"], out["assignmentId"]);
}

async fn vault_with_secret(client: &Client, group: &str, name: &str, enabled_for_templates: bool) -> String {
    let vault = format!("{SUB}/resourceGroups/{group}/providers/Microsoft.KeyVault/vaults/{name}");
    let body = json!({"location": "eastus", "properties": {
        "tenantId": client.config().tenant_id,
        "sku": {"family": "A", "name": "standard"},
        "enabledForTemplateDeployment": enabled_for_templates,
    }});
    let reply = client.arm(Method::PUT, &format!("{vault}?api-version=2023-07-01"), Some(body)).await;
    assert!(reply.status.is_success(), "{}", reply.text());
    let reply = client
        .arm(Method::PUT, &format!("{vault}/secrets/source?api-version=2023-07-01"), Some(json!({"properties": {"value": "from-the-vault"}})))
        .await;
    assert!(reply.status.is_success(), "{}", reply.text());
    vault
}

#[tokio::test]
async fn key_vault_references_in_parameters() {
    let client = Client::new();
    client.create_group("kvref").await;
    vault_with_secret(&client, "kvref", "kvref-vault", true).await;

    // Bicep's getSecret(): a module parameter that references the vault.
    let deployment = deploy(&client, "kvref", "main", template("keyvault-reference"), json!({"vaultName": {"value": "kvref-vault"}})).await;
    assert_eq!(deployment["properties"]["provisioningState"], "Succeeded", "{deployment}");
    let token = client.token("https://vault.azure.localhost:10443/.default").await;
    let reply = client
        .send(
            Method::GET,
            "kvref-vault.vault.azure.localhost:10443",
            "/secrets/copied?api-version=7.4",
            &[("authorization", &format!("Bearer {token}"))],
            "",
        )
        .await;
    assert_eq!(reply.json()["value"], "from-the-vault", "{}", reply.text());

    // A reference given directly, pinned to a version.
    let versions = client
        .send(
            Method::GET,
            "kvref-vault.vault.azure.localhost:10443",
            "/secrets/source/versions?api-version=7.4",
            &[("authorization", &format!("Bearer {token}"))],
            "",
        )
        .await
        .json();
    let version = versions["value"][0]["id"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
    let template = json!({
        "$schema": "https://schema.management.azure.com/schemas/2019-04-01/deploymentTemplate.json#",
        "contentVersion": "1.0.0.0",
        "parameters": {"secret": {"type": "securestring"}},
        "resources": [],
        "outputs": {"length": {"type": "int", "value": "[length(parameters('secret'))]"}},
    });
    let reference = |vault: &str, secret: &str| {
        json!({"secret": {"reference": {
            "keyVault": {"id": format!("{SUB}/resourceGroups/kvref/providers/Microsoft.KeyVault/vaults/{vault}")},
            "secretName": secret,
            "secretVersion": version,
        }}})
    };
    let deployment = deploy(&client, "kvref", "direct", template.clone(), reference("kvref-vault", "source")).await;
    assert_eq!(outputs(&deployment)["length"]["value"], 14);
    assert_eq!(deployment["properties"]["parameters"]["secret"], json!({"type": "SecureString"}));

    // A vault that does not allow template deployment, and a missing secret.
    vault_with_secret(&client, "kvref", "closed-vault", false).await;
    for (vault, secret) in [("closed-vault", "source"), ("kvref-vault", "missing")] {
        let body = json!({"properties": {"mode": "Incremental", "template": template, "parameters": reference(vault, secret)}});
        let reply = client.arm(Method::PUT, &format!("{}?{API}", deployment_path("kvref", "refused")), Some(body)).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text());
        assert_eq!(reply.error_code(), "KeyVaultParameterReferenceSecretRetrieveFailed");
    }
}
