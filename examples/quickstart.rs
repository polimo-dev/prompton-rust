//! Resolve a use case, call a provider, log the model call.
//!
//! ```sh
//! cargo run --example quickstart                       # offline, on a use-case document written inline
//! PTN_API_KEY=ptn_myproject_… cargo run --example quickstart   # against a real PromptOn
//! ```
//!
//! The "provider call" here is a function that makes up an answer. In a real app that is your
//! own HTTP client, with your own provider key: PromptOn is never in the request path.

use std::time::Duration;

use prompton::{
    CallMeta, Client, Completion, CostSource, Error, Message, Mode, Result, Usage, UseCaseOptions,
};
use serde_json::json;

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let live = std::env::var("PTN_API_KEY").is_ok();

    let client = if live {
        // PTN_HOST and PTN_API_KEY come from the environment; everything else is a default.
        Client::builder()
            .cache_ttl(Duration::from_secs(10))
            .build()?
    } else {
        let client = Client::builder()
            .mode(Mode::Test)
            .environment("production")
            .without_disk_cache()
            .build()?;
        client.set_use_cases(&example_use_case_document())?;
        client
    };

    // 1. Which model, which parameters, which prompt — from the use-case document in memory, no HTTP call.
    let call = match client.use_case_with("greeting", &UseCaseOptions::prompt("default")) {
        Ok(call) => call,
        Err(Error::Unresolved(use_case)) => {
            eprintln!("{use_case} has no live deployment in this environment — deploy it first");
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    println!(
        "use_case {} → {} (deployment {} revision {:?}, source {})",
        call.key,
        call.model.clone().unwrap_or_default(),
        call.deployment_id.clone().unwrap_or_default(),
        call.deployment_revision,
        call.source.as_str()
    );

    // 2. Render PromptOn-managed messages, then append app-owned conversation turns.
    let variables = json!({"name": "Ada"});
    let mut messages = call.messages(variables.clone())?;
    messages.extend(app_history());
    messages.push(Message::new("user", "Please greet Ada."));
    for message in &messages {
        println!("  [{}] {}", message.role, message.content);
    }

    // 3. Call the provider yourself, and let the SDK time it and log it.
    let answer = call.track(
        CallMeta::new()
            .variables(variables)
            .input_messages(messages.clone())
            .end_user_ref("user-42")
            .trace_id("example:quickstart")
            .context(json!({"language": "en"})),
        || {
            let (content, tokens) =
                fake_provider(&call.model.clone().unwrap_or_default(), &messages);
            Ok(Completion::new(
                content.clone(),
                Result::text(content).with_finish_reason("stop").with_usage(
                    Usage::tokens(tokens.0, tokens.1).with_cost(0.000012, CostSource::Provider),
                ),
            ))
        },
    )?;
    println!("provider answered: {}", answer.value);

    // 4. Monitoring logs are batched; flush before a short-lived process exits.
    if live {
        let flushed = client.flush()?;
        println!("sent {} monitoring log(s)", flushed.accepted);
    } else {
        for record in client.captured_logs() {
            println!(
                "would have logged: {}",
                serde_json::to_string_pretty(&record)?
            );
        }
    }

    Ok(())
}

/// Stands in for your provider SDK: returns the answer and (input, output) token counts.
fn fake_provider(model: &str, messages: &[Message]) -> (String, (i64, i64)) {
    let prompt: String = messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let words = prompt.split_whitespace().count() as i64;
    (format!("Hello! (pretending to be {model})"), (words, 8))
}

fn app_history() -> Vec<Message> {
    vec![Message::new("user", "My name is Ada.")]
}

/// The use-case document an app would normally fetch from PromptOn, inline so the example runs anywhere.
fn example_use_case_document() -> serde_json::Value {
    json!({
        "schema_version": 4,
        "project": "example",
        "environment": "production",
        "use_cases": {
            "greeting": {
                "id": "0198f2a1-0000-7000-8000-00000000c001",
                "kind": "chat",
                "input_schema": [{"name": "name", "type": "string", "required": true}],
                "default_params": {"max_tokens": 512, "temperature": 0.7},
                "payload_policy": {"mode": "full", "sample_rate": 1.0, "max_bytes": 262144}
            }
        },
        "deployments": {
            "greeting": {
                "id": "0198f2a1-0000-7000-8000-00000000d001",
                "revision": "v2026.09.30-3",
                "model_id": "0198f2a1-0000-7000-8000-00000000e001",
                "params": {"temperature": 0.2},
                "provider_options": {},
                "prompt_pins": {"default": "0198f2a1-0000-7000-8000-00000000a001"}
            }
        },
        "prompt_versions": {
            "0198f2a1-0000-7000-8000-00000000a001": {
                "id": "0198f2a1-0000-7000-8000-00000000a001",
                "number": 2,
                "engine": "liquid",
                "messages": [
                    {"role": "system", "content": "You are a friendly greeter. Answer in one line."},
                    {"role": "user", "content": "Say hello to {{ name }}."}
                ]
            }
        },
        "models": {
            "0198f2a1-0000-7000-8000-00000000e001": {
                "id": "0198f2a1-0000-7000-8000-00000000e001",
                "provider": "openrouter",
                "model_id": "openai/gpt-4o-mini",
                "display_name": "GPT-4o mini",
                "provider_options": {"only": ["OpenAI"]},
                "capabilities": ["tools"],
                "status": "active"
            }
        }
    })
}
