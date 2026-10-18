use castor::config::{Config, load_with};
use castor::engine::provider::{EngineClient, Message, ToolSchema};
use castor::mcp::dgi::evaluate_configured_model_probe;
use serde_json::json;
use std::path::PathBuf;
use std::time::Instant;

fn get_client() -> (Config, EngineClient) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".scratch/bonsai-eval")
        .join(format!("{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();

    let config = load_with(|key| match key {
        "CASTOR_STATE_DIR" => Some(root.to_string_lossy().into()),
        "CASTOR_ENGINE_TYPE" => Some("llama.cpp".into()),
        "CASTOR_BASE_URL" => Some("http://127.0.0.1:18020/v1".into()),
        "CASTOR_MODEL" => Some("Qwen3.8-27B".into()),
        _ => None,
    })
    .unwrap()
    .config;

    let client = EngineClient::from_config(&config).expect("failed to create client");
    (config, client)
}

#[tokio::test]
async fn test_1_chat_and_thinking_trace() {
    let (_config, client) = get_client();
    let messages = vec![Message {
        role: "user".into(),
        content: "What is 15 * 17? Give the final number.".into(),
        ..Default::default()
    }];

    let start = Instant::now();
    let res = client
        .chat(&messages, &[], true, Some("medium"))
        .await
        .expect("chat failed");
    let elapsed = start.elapsed();

    println!("\n=== TEST 1: CHAT & THINKING TRACE ===");
    println!("Elapsed: {:?}", elapsed);
    println!("Content:\n{}", res.content);
    println!(
        "Tokens: prompt={:?}, completion={:?}, speed={:?} tok/s",
        res.metrics.prompt_tokens, res.metrics.completion_tokens, res.metrics.tokens_per_sec
    );

    assert!(
        res.content.contains("255"),
        "Model failed arithmetic: {}",
        res.content
    );
}

#[tokio::test]
async fn test_2_reasoning_effort_tiers() {
    let (_config, client) = get_client();
    println!("\n=== TEST 2: REASONING EFFORT TIERS ===");

    for tier in &["medium", "low", "xhigh"] {
        let messages = vec![Message {
            role: "user".into(),
            content: "Name the capital of France. One word only.".into(),
            ..Default::default()
        }];
        let start = Instant::now();
        let res = client.chat(&messages, &[], true, Some(tier)).await;
        match res {
            Ok(r) => println!(
                "Tier '{}': OK ({:?} completion tokens in {:?})",
                tier,
                r.metrics.completion_tokens,
                start.elapsed()
            ),
            Err(e) => println!("Tier '{}': FAILED ({})", tier, e),
        }
    }

    // Test unsupported "high" tier
    let messages = vec![Message {
        role: "user".into(),
        content: "Hello".into(),
        ..Default::default()
    }];
    let high_res = client.chat(&messages, &[], true, Some("high")).await;
    println!(
        "Tier 'high' (expected to fail with HTTP error): {:?}",
        high_res.is_err()
    );
}

#[tokio::test]
async fn test_3_tool_call_with_arguments() {
    let (_config, client) = get_client();
    let messages = vec![Message {
        role: "user".into(),
        content: "Please read lines 1 to 5 of file 'Cargo.toml' using read_file.".into(),
        ..Default::default()
    }];
    let tools = vec![ToolSchema {
        name: "read_file".into(),
        description: "Read a slice of lines from a file".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": { "type": "integer" },
                "end_line": { "type": "integer" }
            },
            "required": ["path"]
        }),
    }];

    println!("\n=== TEST 3: TOOL CALL WITH ARGUMENTS ===");
    let start = Instant::now();
    let res = client
        .chat(&messages, &tools, true, Some("medium"))
        .await
        .expect("tool call failed");
    println!("Elapsed: {:?}", start.elapsed());
    println!("Content:\n{}", res.content);
    println!("Tool calls count: {}", res.tool_calls.len());

    assert!(
        !res.tool_calls.is_empty(),
        "Model failed to produce tool calls: {}",
        res.content
    );
    let call = &res.tool_calls[0];
    println!("Tool name: {}", call.name);
    println!("Tool args: {}", call.arguments);
    assert_eq!(call.name, "read_file");

    let parsed_args: serde_json::Value =
        serde_json::from_str(&call.arguments).expect("Arguments must be valid JSON");
    assert!(
        parsed_args.get("path").is_some(),
        "Path arg missing in {}",
        call.arguments
    );
}

#[tokio::test]
async fn test_4_multi_turn_tool_loop() {
    let (_config, client) = get_client();
    let mut messages = vec![Message {
        role: "user".into(),
        content: "Use query_database to get user 101, then tell me their role.".into(),
        ..Default::default()
    }];
    let tools = vec![ToolSchema {
        name: "query_database".into(),
        description: "Query database for user info".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "user_id": { "type": "integer" }
            },
            "required": ["user_id"]
        }),
    }];

    println!("\n=== TEST 4: MULTI-TURN TOOL LOOP ===");
    let first = client
        .chat(&messages, &tools, true, Some("medium"))
        .await
        .expect("first turn failed");
    assert!(
        !first.tool_calls.is_empty(),
        "Model did not invoke query_database"
    );

    messages.push(Message {
        role: "assistant".into(),
        content: first.content,
        tool_calls: first.tool_calls.clone(),
        ..Default::default()
    });

    for call in &first.tool_calls {
        messages.push(Message {
            role: "tool".into(),
            content: json!({"user_id": 101, "name": "Alice", "role": "Lead Architect"}).to_string(),
            tool_call_id: Some(call.id.clone()),
            ..Default::default()
        });
    }

    let second = client
        .chat(&messages, &tools, true, Some("medium"))
        .await
        .expect("second turn failed");
    println!("Second turn content:\n{}", second.content);
    assert!(
        second.content.contains("Lead Architect") || second.content.contains("Architect"),
        "Failed to extract role: {}",
        second.content
    );
}

#[tokio::test]
async fn test_5_dgi_constrained_probe() {
    let (config, _client) = get_client();
    let http = reqwest::Client::new();
    println!("\n=== TEST 5: DGI CONSTRAINED PROBE ===");

    // A fast, decomposed prompt
    let prompt_fast = "Fix the typo in line 42 of src/config.rs: change 'parm' to 'param'.";
    let start = Instant::now();
    let verdict_fast = evaluate_configured_model_probe(&config, prompt_fast, &http).await;
    println!(
        "Decomposed prompt probe result in {:?}: {:?}",
        start.elapsed(),
        verdict_fast
    );

    // An overloaded monolithic prompt
    let prompt_heavy = "Completely rewrite the entire architecture of the networking subsystem, replace tokio with custom epoll, refactor all 40 files, and write benchmarks.";
    let start_heavy = Instant::now();
    let verdict_heavy = evaluate_configured_model_probe(&config, prompt_heavy, &http).await;
    println!(
        "Monolithic prompt probe result in {:?}: {:?}",
        start_heavy.elapsed(),
        verdict_heavy
    );
}
