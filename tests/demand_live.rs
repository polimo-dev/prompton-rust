use std::time::Duration;

use prompton::Client;
use serde_json::Value;

#[test]
fn live_demand_contract() {
    let path = match std::env::var("PTN_LIVE_CREDENTIALS") {
        Ok(path) => path,
        Err(_) => return,
    };
    let data = std::fs::read(path).expect("read live credentials");
    let creds: Value = serde_json::from_slice(&data).expect("decode live credentials");
    let base_url = creds["base_url"].as_str().expect("base_url");
    let api_key = creds["api_key"].as_str().expect("api_key");
    let environment = creds["environment"].as_str().unwrap_or("production");
    let project = creds["project"].as_str().unwrap_or("sdk-demand-contract");
    let prompt_key = creds["prompt_key"].as_str().unwrap_or("demand_greeting");
    let other_key = creds["other_prompt_key"].as_str().unwrap_or("demand_other");
    let variables = creds
        .get("variables")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({"name": "World"}));

    let client = Client::builder()
        .host(base_url)
        .api_key(api_key)
        .environment(environment)
        .project(project)
        .cache_ttl(Duration::from_secs(10))
        .without_disk_cache()
        .log_sink(|_| {})
        .build()
        .expect("client");

    let cold = render(&client, prompt_key, variables.clone());
    let fresh = render(&client, prompt_key, variables.clone());
    let other = render(&client, other_key, variables);
    assert_eq!(cold, "Hello World");
    assert_eq!(fresh, "Hello World");
    assert_eq!(other, "Hello World");
}

fn render(client: &Client, key: &str, variables: Value) -> String {
    let use_case = client.use_case(key).expect("use_case");
    let messages = use_case.messages(variables).expect("messages");
    messages.last().expect("message").content.clone()
}
