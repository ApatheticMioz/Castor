//! Offline protocol and separate-process lifecycle checks for llama.cpp.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::{
    Json, Router,
    routing::{get, post},
};
use castor::config::{Config, load_with};
use castor::engine::{EngineClient, EngineLifecycle, Message, ToolSchema};
use castor::state::StateDir;
use serde_json::{Value, json};

static NEXT: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
    config: Config,
}

impl Fixture {
    fn new(base: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(".scratch/llama-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let config = load_with(|key| match key {
            "CASTOR_STATE_DIR" => Some(root.to_string_lossy().into()),
            "CASTOR_ENGINE_TYPE" => Some("llama.cpp".into()),
            "CASTOR_BASE_URL" => Some(base.into()),
            "CASTOR_MODEL" => Some("fixture".into()),
            _ => None,
        })
        .unwrap()
        .config;
        Self { root, config }
    }

    fn cli(&self, action: &str) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_castor"));
        // Pin every relevant field so a developer's live-engine settings cannot leak in.
        command
            .args(["server", action])
            .env("CASTOR_STATE_DIR", &self.root)
            .env("CASTOR_ENGINE_TYPE", "llama.cpp")
            .env("CASTOR_BASE_URL", self.config.base_url.as_ref().unwrap())
            .env("CASTOR_MODEL", "fixture")
            .env(
                "CASTOR_LAUNCH_COMMAND",
                self.config.launch_command.as_deref().unwrap_or(""),
            )
            .env(
                "CASTOR_STOP_COMMAND",
                self.config.stop_command.as_deref().unwrap_or(""),
            )
            .env(
                "CASTOR_API_KEY",
                self.config.api_key.as_deref().unwrap_or(""),
            )
            .env(
                "CASTOR_BOOT_TIMEOUT_SECS",
                self.config.boot_timeout_secs.to_string(),
            )
            .env("ALLOW_ENGINE_INTERRUPT", "0");
        command
    }

    fn manage(&mut self, port: u16) {
        let exe = std::env::current_exe().unwrap();
        self.config.launch_command = Some(format!(
            "\"{}\" --exact fixture_server --nocapture",
            exe.display()
        ));
        self.config.boot_timeout_secs = 3;
        std::fs::write(self.root.join("fixture-port"), port.to_string()).unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.root.join("llama-owner.json").exists() {
            let _ = self
                .cli("stop")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        // Keep failure artifacts for inspection; scratch is never committed.
    }
}

fn text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

async fn mock(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}/v1")
}

fn readiness_routes() -> Router {
    Router::new()
        .route("/v1/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route(
            "/v1/models",
            get(|| async { Json(json!({"data":[{"id":"fixture"}]})) }),
        )
}

#[tokio::test]
async fn fixture_server() {
    let Ok(root) = std::env::var("CASTOR_STATE_DIR") else {
        return;
    };
    let port_path = PathBuf::from(root).join("fixture-port");
    if !port_path.exists() {
        return;
    }
    let port: u16 = std::fs::read_to_string(port_path).unwrap().parse().unwrap();
    if std::env::var("CASTOR_LLAMA_FIXTURE_FAIL").as_deref() == Ok("1") {
        eprintln!("fixture launch failure");
        std::process::exit(7);
    }
    if std::env::var("CASTOR_LLAMA_FIXTURE_LOAD").as_deref() == Ok("1") {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let app = Router::new().route(
            "/v1/health",
            get(|| async {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "fixture loading model",
                )
            }),
        );
        axum::serve(listener, app).await.unwrap();
        return;
    }
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    axum::serve(listener, readiness_routes()).await.unwrap();
}

fn available_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[test]
fn managed_start_status_stop_across_invocations() {
    let port = available_port();
    let mut fixture = Fixture::new(&format!("http://127.0.0.1:{port}/v1"));
    fixture.manage(port);
    let started = fixture.cli("start").output().unwrap();
    assert!(started.status.success(), "{}", text(&started));
    let healthy = fixture.cli("status").output().unwrap();
    assert!(healthy.status.success(), "{}", text(&healthy));
    let again = fixture.cli("start").output().unwrap();
    assert!(again.status.success(), "{}", text(&again));
    assert!(text(&again).contains("already running"));
    let owner: Value =
        serde_json::from_slice(&std::fs::read(fixture.root.join("llama-owner.json")).unwrap())
            .unwrap();
    let mut unauthorized = std::net::TcpStream::connect((
        std::net::Ipv4Addr::LOCALHOST,
        owner["control_port"].as_u64().unwrap() as u16,
    ))
    .unwrap();
    use std::io::Write;
    unauthorized
        .write_all(b"{\"token\":\"wrong\",\"action\":\"stop\"}\n")
        .unwrap();
    drop(unauthorized);
    assert!(fixture.cli("status").output().unwrap().status.success());
    let stopped = fixture.cli("stop").output().unwrap();
    assert!(stopped.status.success(), "{}", text(&stopped));
    assert!(!fixture.root.join("llama-owner.json").exists());
    assert!(std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).is_err());
    // Shutdown acknowledgement must precede the next launch without a record race.
    assert!(fixture.cli("start").output().unwrap().status.success());
    assert!(fixture.cli("stop").output().unwrap().status.success());
}

#[test]
fn launch_exit_and_timeout_preserve_errors_and_cleanup() {
    for (flag, expected) in [
        ("CASTOR_LLAMA_FIXTURE_FAIL", "fixture launch failure"),
        ("CASTOR_LLAMA_FIXTURE_LOAD", "fixture loading model"),
    ] {
        let port = available_port();
        let mut fixture = Fixture::new(&format!("http://127.0.0.1:{port}/v1"));
        fixture.manage(port);
        let failed = fixture.cli("start").env(flag, "1").output().unwrap();
        assert!(!failed.status.success(), "{}", text(&failed));
        assert!(text(&failed).contains(expected), "{}", text(&failed));
        assert!(!fixture.root.join("llama-owner.json").exists());
        assert!(std::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).is_err());
    }
}

#[test]
fn concurrent_start_does_not_create_two_owners() {
    let port = available_port();
    let mut fixture = Fixture::new(&format!("http://127.0.0.1:{port}/v1"));
    fixture.manage(port);
    let first = fixture
        .cli("start")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let second = fixture.cli("start").output().unwrap();
    let first = first.wait_with_output().unwrap();
    assert!(
        first.status.success() || second.status.success(),
        "{} {}",
        text(&first),
        text(&second)
    );
    let unhealthy = if first.status.success() {
        &second
    } else {
        &first
    };
    if !unhealthy.status.success() {
        assert!(
            text(unhealthy).contains("boot lock held"),
            "{}",
            text(unhealthy)
        );
    }
    assert!(fixture.cli("status").output().unwrap().status.success());
    assert!(fixture.cli("stop").output().unwrap().status.success());
}

#[test]
fn stale_identity_and_unowned_stop_are_refused() {
    let fixture = Fixture::new("http://127.0.0.1:1/v1");
    let unowned = fixture.cli("stop").output().unwrap();
    assert!(!unowned.status.success());
    assert!(text(&unowned).contains("no owned llama.cpp server"));
    std::fs::write(fixture.root.join("llama-owner.json"), json!({
        "pid":std::process::id(), "identity":"wrong-birth-identity", "endpoint":"http://127.0.0.1:1/v1",
        "model":"fixture", "control_port":1, "token":"invalid"
    }).to_string()).unwrap();
    let stale = fixture.cli("stop").output().unwrap();
    assert!(!stale.status.success());
    assert!(
        text(&stale).contains("birth identity changed"),
        "{}",
        text(&stale)
    );
}

#[tokio::test]
async fn external_server_readiness_authentication_and_alias() {
    let base = mock(readiness_routes().route_layer(axum::middleware::from_fn(
        |request: axum::extract::Request, next: axum::middleware::Next| async move {
            assert_eq!(request.headers()["authorization"], "Bearer fixture-key");
            next.run(request).await
        },
    )))
    .await;
    let mut fixture = Fixture::new(&base);
    fixture.config.api_key = Some("fixture-key".into());
    let lifecycle = EngineLifecycle::new(&fixture.config, &StateDir::new(&fixture.root));
    lifecycle.ensure_running().await.unwrap();
    let error = lifecycle.stop().await.unwrap_err().to_string();
    assert!(error.contains("no owned llama.cpp server"));
    fixture.config.model = Some("wrong-alias".into());
    let error = EngineLifecycle::new(&fixture.config, &StateDir::new(&fixture.root))
        .readiness()
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("--alias"), "{error}");
}

#[tokio::test]
async fn health_loading_and_http_errors_are_visible() {
    let base = mock(Router::new().route(
        "/v1/health",
        get(|| async { (axum::http::StatusCode::SERVICE_UNAVAILABLE, "Loading model") }),
    ))
    .await;
    let fixture = Fixture::new(&base);
    let lifecycle = EngineLifecycle::new(&fixture.config, &StateDir::new(&fixture.root));
    let error = lifecycle.ensure_running().await.unwrap_err().to_string();
    assert!(
        error.contains("503") && error.contains("Loading model"),
        "{error}"
    );
}

#[tokio::test]
async fn prefix_and_json_configuration_are_preserved() {
    let base = mock(Router::new().nest("/prefix", readiness_routes()))
        .await
        .replace("/v1", "/prefix/v1/");
    let fixture = Fixture::new(&base);
    std::fs::write(
        fixture.root.join("config.json"),
        json!({"engine_type":"llama.cpp", "base_url":base, "model":"fixture"}).to_string(),
    )
    .unwrap();
    let loaded = load_with(|key| {
        if key == "CASTOR_STATE_DIR" {
            Some(fixture.root.to_string_lossy().into())
        } else {
            None
        }
    })
    .unwrap();
    assert_eq!(loaded.config.engine_type.as_deref(), Some("llama.cpp"));
    assert_eq!(loaded.sources.engine_type, castor::config::Source::File);
    EngineLifecycle::new(&loaded.config, &StateDir::new(&fixture.root))
        .readiness()
        .await
        .unwrap();
    let overridden = load_with(|key| match key {
        "CASTOR_STATE_DIR" => Some(fixture.root.to_string_lossy().into()),
        "CASTOR_ENGINE_TYPE" => Some("vllm".into()),
        _ => None,
    })
    .unwrap();
    assert_eq!(overridden.config.engine_type.as_deref(), Some("vllm"));
    assert_eq!(overridden.sources.engine_type, castor::config::Source::Env);
}

#[tokio::test]
async fn malformed_health_and_wrong_model_are_not_ready() {
    for body in ["not json", "{\"status\":\"loading\"}"] {
        let base = mock(Router::new().route("/v1/health", get(move || async move { body }))).await;
        let fixture = Fixture::new(&base);
        let error = EngineLifecycle::new(&fixture.config, &StateDir::new(&fixture.root))
            .readiness()
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("malformed") || error.contains("loading"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn gate_http_failure_preserves_status_and_body() {
    let base = mock(Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                "fixture authentication required",
            )
        }),
    ))
    .await;
    let fixture = Fixture::new(&base);
    let error = castor::mcp::dgi::evaluate_configured_model_probe(
        &fixture.config,
        "task",
        &reqwest::Client::new(),
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("401") && error.contains("fixture authentication required"),
        "{error}"
    );
}

#[tokio::test]
async fn constrained_gate_validates_verdicts_and_request() {
    for verdict in ["ADMIT", "OVERLOADED", "INVALID", "EXTRA"] {
        let base = mock(Router::new().route("/v1/chat/completions", post(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer fixture-key");
            assert_eq!(body["reasoning_effort"], "none");
            assert_eq!(body["max_tokens"], 64);
            assert!(body.get("guided_choice").is_none());
            assert!(body.get("prompt").is_none());
            assert_eq!(body["messages"][1]["content"], "task");
            assert_eq!(body["response_format"]["schema"]["properties"]["verdict"]["enum"], json!(["ADMIT", "OVERLOADED"]));
            let content = if verdict == "EXTRA" { json!({"verdict":"ADMIT", "extra":true}) } else { json!({"verdict":verdict}) };
            Json(json!({"choices":[{"finish_reason":"stop", "message":{"content":content.to_string()}}]}))
        }))).await;
        let mut fixture = Fixture::new(&base);
        fixture.config.api_key = Some("fixture-key".into());
        let result = castor::mcp::dgi::evaluate_configured_model_probe(
            &fixture.config,
            "task",
            &reqwest::Client::new(),
        )
        .await;
        match verdict {
            "ADMIT" => assert_eq!(result.unwrap(), castor::mcp::dgi::DgiVerdict::Admit),
            "OVERLOADED" => assert!(result.unwrap().is_reject()),
            _ => assert!(result.is_err()),
        }
    }
}

#[tokio::test]
async fn llama_chat_reasoning_and_tool_history() {
    let base = mock(Router::new().route("/v1/chat/completions", post(|Json(body): Json<Value>| async move {
        assert!(body.get("chat_template_kwargs").is_none());
        assert_eq!(body["tools"][0]["function"]["name"], "inspect");
        assert_eq!(body["messages"][1]["tool_call_id"], "call-one");
        assert_eq!(body["messages"][0]["tool_calls"][0]["id"], "call-one");
        Json(json!({"choices":[{"finish_reason":"stop","message":{"content":body.to_string()}}]}))
    }))).await;
    let fixture = Fixture::new(&base);
    let client = EngineClient::from_config(&fixture.config).unwrap();
    let messages = vec![
        Message {
            role: "assistant".into(),
            tool_calls: vec![castor::engine::ToolCall {
                id: "call-one".into(),
                name: "inspect".into(),
                arguments: "{}".into(),
                ..Default::default()
            }],
            ..Default::default()
        },
        Message {
            role: "tool".into(),
            content: "found".into(),
            tool_call_id: Some("call-one".into()),
            ..Default::default()
        },
    ];
    let tools = vec![ToolSchema {
        name: "inspect".into(),
        description: "inspect".into(),
        parameters: json!({"type":"object"}),
    }];
    for effort in [Some("low"), Some("medium"), Some("xhigh"), None] {
        let completion = client.chat(&messages, &tools, false, effort).await.unwrap();
        let body: Value = serde_json::from_str(&completion.content).unwrap();
        assert_eq!(body.get("reasoning_effort").and_then(Value::as_str), effort);
    }
}

#[tokio::test]
async fn fragmented_stream_has_multiple_tools_utf8_and_usage() {
    use axum::body::{Body, Bytes};
    use axum::response::IntoResponse;
    let base = mock(Router::new().route("/v1/chat/completions", post(|Json(body): Json<Value>| async move {
        assert_eq!(body["stream_options"]["include_usage"], true);
        let frames = [
            json!({"choices":[{"delta":{"content":"héllo", "tool_calls":[{"index":0,"id":"one","function":{"name":"inspect", "arguments":"{\"x\":"}}, {"index":1,"id":"two","function":{"name":"inspect", "arguments":"{}"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":3}}}),
        ];
        let mut data = frames.iter().map(|f| format!("data: {f}\n\n")).collect::<String>();
        data.push_str("data: [DONE]\n\n");
        let chunks = data.into_bytes().into_iter().map(|b| Ok::<_, std::convert::Infallible>(Bytes::from(vec![b]))).collect::<Vec<_>>();
        ([("content-type", "text/event-stream")], Body::from_stream(futures_util::stream::iter(chunks))).into_response()
    }))).await;
    let fixture = Fixture::new(&base);
    let completion = EngineClient::from_config(&fixture.config)
        .unwrap()
        .chat(
            &[Message {
                role: "user".into(),
                content: "hi".into(),
                ..Default::default()
            }],
            &[],
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(completion.content, "héllo");
    assert_eq!(completion.tool_calls.len(), 2);
    assert_eq!(completion.tool_calls[0].arguments, "{\"x\":1}");
    assert_eq!(completion.tool_calls[1].id, "two");
    assert_eq!(completion.metrics.prompt_tokens, Some(10));
    assert_eq!(completion.metrics.cached_tokens, Some(3));
    assert_eq!(completion.metrics.completion_tokens, Some(5));
}

#[tokio::test]
async fn streaming_errors_and_disconnects_are_not_completions() {
    for data in [
        "data: {\"error\":{\"message\":\"fixture slot failed\"}}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
    ] {
        let base = mock(Router::new().route(
            "/v1/chat/completions",
            post(move || async move { ([("content-type", "text/event-stream")], data) }),
        ))
        .await;
        let fixture = Fixture::new(&base);
        let error = EngineClient::from_config(&fixture.config)
            .unwrap()
            .chat(&[], &[], true, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("fixture slot failed") || error.contains("without [DONE]"),
            "{error}"
        );
    }
}

/// Attach to an already running server; this test never starts or stops it.
#[tokio::test]
#[ignore = "requires CASTOR_LLAMA_EXISTING_BASE_URL, CASTOR_LLAMA_EXISTING_MODEL and ALLOW_ENGINE_INTERRUPT=1"]
async fn live_existing_llama_cpp_tool_round_trip() {
    assert_eq!(std::env::var("ALLOW_ENGINE_INTERRUPT").as_deref(), Ok("1"));
    let base = std::env::var("CASTOR_LLAMA_EXISTING_BASE_URL")
        .expect("set CASTOR_LLAMA_EXISTING_BASE_URL");
    let model =
        std::env::var("CASTOR_LLAMA_EXISTING_MODEL").expect("set CASTOR_LLAMA_EXISTING_MODEL");
    let mut fixture = Fixture::new(&base);
    fixture.config.model = Some(model);
    fixture.config.api_key = std::env::var("CASTOR_API_KEY").ok();
    EngineLifecycle::new(&fixture.config, &StateDir::new(&fixture.root))
        .readiness()
        .await
        .unwrap();
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap();
    let verdict = castor::mcp::dgi::evaluate_configured_model_probe(
        &fixture.config,
        "Use the get_number tool to retrieve a number and report it.",
        &http,
    )
    .await
    .unwrap();
    assert_eq!(verdict, castor::mcp::dgi::DgiVerdict::Admit);
    assert_tool_round_trip(&fixture.config).await;
}

async fn assert_tool_round_trip(config: &Config) {
    let client = EngineClient::from_config(config).unwrap();
    let mut messages = vec![Message {
        role: "user".into(),
        content: "Call the get_number tool, then tell me the returned number.".into(),
        ..Default::default()
    }];
    let tools = vec![ToolSchema {
        name: "get_number".into(),
        description: "Return a number. Call this to obtain the requested number.".into(),
        parameters: json!({"type":"object", "properties":{}, "additionalProperties":false}),
    }];
    let first = tokio::time::timeout(
        Duration::from_secs(120),
        client.chat(&messages, &tools, true, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        !first.tool_calls.is_empty(),
        "model did not call get_number: {}",
        first.content
    );
    messages.push(Message {
        role: "assistant".into(),
        content: first.content,
        tool_calls: first.tool_calls.clone(),
        ..Default::default()
    });
    for call in first.tool_calls {
        assert_eq!(call.name, "get_number");
        messages.push(Message {
            role: "tool".into(),
            content: "{\"number\":42}".into(),
            tool_call_id: Some(call.id),
            ..Default::default()
        });
    }
    let second = tokio::time::timeout(
        Duration::from_secs(120),
        client.chat(&messages, &[], true, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(second.content.contains("42"), "{}", second.content);
}

/// Run explicitly with a user-installed, tool-capable GGUF and llama-server.
#[tokio::test]
#[ignore = "requires CASTOR_LLAMA_MODEL_FILE, CASTOR_LLAMA_SERVER and ALLOW_ENGINE_INTERRUPT=1"]
async fn live_llama_cpp_tool_round_trip() {
    assert_eq!(std::env::var("ALLOW_ENGINE_INTERRUPT").as_deref(), Ok("1"));
    let model = std::env::var("CASTOR_LLAMA_MODEL_FILE").expect("set CASTOR_LLAMA_MODEL_FILE");
    let server = std::env::var("CASTOR_LLAMA_SERVER").expect("set CASTOR_LLAMA_SERVER");
    let port = available_port();
    let mut fixture = Fixture::new(&format!("http://127.0.0.1:{port}/v1"));
    assert!(
        !server.contains('"') && !model.contains('"'),
        "paths cannot contain shell quotes"
    );
    fixture.config.launch_command = Some(format!(
        "\"{server}\" -m \"{model}\" --host 127.0.0.1 --port {port} --alias fixture --jinja -c 4096 -np 1"
    ));
    fixture.config.boot_timeout_secs = 180;
    let output = fixture.cli("start").output().unwrap();
    assert!(output.status.success(), "{}", text(&output));
    assert_tool_round_trip(&fixture.config).await;
    assert!(fixture.cli("stop").output().unwrap().status.success());
}
