//! 端到端：代理 + 账号池轮换 + 上游错误矩阵。
//!
//! 这些测试把 mock 上游真正跑起来（随机端口的 axum 服务器），再让本地代理
//! 打过去——覆盖 docs/PLAN.md 第 8.1 节的错误矩阵，这是全项目风险最集中的一段。

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use cc_server::mock_upstream::{MockResponse, MockUpstream};
use cc_server::pool::AccountSlot;
use cc_server::{router, Config, ProxyState, RequestRecord};
use serde_json::{json, Value};
use tower::ServiceExt;

/// 造一个指向 mock 上游的配置。
fn config_for(base: &str) -> Config {
    Config {
        api_base: base.to_string(),
        // 强制走 CLI 通道：mock 只实现了 /alpha/generate 的 NDJSON
        upstream_protocol: cc_server::UpstreamProtocol::Cli,
        request_timeout: std::time::Duration::from_secs(5),
        stream_idle_timeout: std::time::Duration::from_millis(500),
        ..Config::default()
    }
}

/// 两个账号，key 分别是 key-default / key-second。
fn two_slots() -> Vec<AccountSlot> {
    vec![
        AccountSlot {
            id: "default".into(),
            label: "Default".into(),
        },
        AccountSlot {
            id: "second".into(),
            label: "Second".into(),
        },
    ]
}

/// 两个槽位都从内存里取 key。
fn resolver() -> cc_server::KeyResolver {
    Arc::new(|slot: &AccountSlot| Some(format!("key-{}", slot.id)))
}

/// 发一次对话请求，返回 (状态码, 响应体文本)。
async fn post_chat(state: Arc<ProxyState>, body: Value) -> (StatusCode, String) {
    post_to(state, "/v1/chat/completions", body).await
}

/// 向指定路径发一次 POST，返回 (状态码, 响应体文本)。
async fn post_to(state: Arc<ProxyState>, uri: &str, body: Value) -> (StatusCode, String) {
    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// 从 SSE 文本里拼出全部 content。
fn concat_content(sse: &str) -> String {
    let mut out = String::new();
    for line in sse.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if payload == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(text) = value["choices"][0]["delta"]["content"].as_str() {
            out.push_str(text);
        }
    }
    out
}

#[tokio::test]
async fn happy_path_streams_content_from_the_first_account() {
    let mock = MockUpstream::start([MockResponse::StreamSuccess {
        text: "hello world".into(),
    }])
    .await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let (status, body) = post_chat(
        state,
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.ends_with("data: [DONE]\n\n"), "流必须以 [DONE] 结束");
    assert_eq!(concat_content(&body), "hello world");
    assert_eq!(mock.generate_count(), 1, "首选账号可用时不应换号");
}

#[tokio::test]
async fn non_streaming_request_is_buffered_locally() {
    let mock = MockUpstream::start([MockResponse::StreamSuccess {
        text: "buffered".into(),
    }])
    .await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let (status, body) = post_chat(
        state,
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    // 上游只有流式接口，但客户端要的是非流式 → 本地缓冲成一个完整 JSON
    let value: Value = serde_json::from_str(&body).expect("非流式应返回 JSON 而非 SSE");
    assert_eq!(value["object"], "chat.completion");
    assert_eq!(value["choices"][0]["message"]["content"], "buffered");
}

#[tokio::test]
async fn rotates_to_the_second_account_on_401() {
    // 第一个账号 401，第二个成功
    let mock = MockUpstream::start([
        MockResponse::HttpError {
            status: 401,
            body: r#"{"error":{"code":"invalid_api_key"}}"#.into(),
        },
        MockResponse::StreamSuccess {
            text: "from second".into(),
        },
    ])
    .await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let (status, body) = post_chat(
        state,
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        concat_content(&body),
        "from second",
        "第一次 401 后应换号并成功"
    );
    assert_eq!(mock.generate_count(), 2, "应恰好尝试两个账号");
    let tokens: Vec<String> = mock
        .generate_requests()
        .iter()
        .filter_map(|r| r.bearer_token().map(str::to_string))
        .collect();
    assert_eq!(
        tokens,
        vec!["key-default".to_string(), "key-second".to_string()]
    );
}

#[tokio::test]
async fn rotates_on_429_and_reports_exhaustion_when_all_fail() {
    // 两个账号都 429：应换号一次，然后用尽后报告
    let mock = MockUpstream::start([MockResponse::HttpError {
        status: 429,
        body: r#"{"error":{"code":"rate_limited"}}"#.into(),
    }])
    .await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let (status, body) = post_chat(
        state,
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["error"]["code"], "rate_limit_exceeded");
    assert_eq!(mock.generate_count(), 2, "池里每个账号都应被尝试一次");
}

#[tokio::test]
async fn plan_error_does_not_rotate() {
    // 403 模型不在套餐：换号无用，必须只打一次
    let mock = MockUpstream::start([MockResponse::HttpError {
        status: 403,
        body: r#"{"error":{"code":"MODEL_NOT_IN_PLAN"}}"#.into(),
    }])
    .await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let (status, _body) = post_chat(
        state,
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        mock.generate_count(),
        1,
        "403 套餐错误换号无用；换号会把整个账号池误标为耗尽（PLAN.md 8.1）"
    );
}

#[tokio::test]
async fn upgrade_required_switches_to_cli_transport_without_burning_an_account() {
    // 配置成先走 Provider API：403 upgrade_required 后应降级到 CLI 并原样重试
    let mock = MockUpstream::start([
        MockResponse::UpgradeRequired,
        MockResponse::StreamSuccess {
            text: "via cli".into(),
        },
    ])
    .await;
    let config = Config {
        api_base: mock.base_url().to_string(),
        upstream_protocol: cc_server::UpstreamProtocol::Auto,
        request_timeout: std::time::Duration::from_secs(5),
        stream_idle_timeout: std::time::Duration::from_millis(500),
        ..Config::default()
    };
    let state = Arc::new(ProxyState::new(config, two_slots(), resolver()).unwrap());

    let (status, body) = post_chat(
        state,
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(concat_content(&body), "via cli");
    assert_eq!(mock.generate_count(), 2);
    let tokens: Vec<String> = mock
        .generate_requests()
        .iter()
        .filter_map(|r| r.bearer_token().map(str::to_string))
        .collect();
    assert_eq!(
        tokens,
        vec!["key-default".to_string(), "key-default".to_string()],
        "降级到 CLI 通道不该换账号（Go 套餐的所有账号都只有 CLI 通道）"
    );
    assert!(mock.generate_requests()[1].path.contains("/alpha/generate"));
}

#[tokio::test]
async fn mid_stream_break_is_not_replayed() {
    // 200 之后断流：不能换号重放（客户端已经收到部分内容）
    let mock = MockUpstream::start([MockResponse::BrokenMidStream {
        text: "partial".into(),
    }])
    .await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let (status, body) = post_chat(
        state,
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;

    // 响应头已经是 200（流已开始），错误只能作为流内事件告知
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("partial"), "断流前的内容应已下发：{body}");
    assert!(
        body.contains("error"),
        "断流应以流内 error 事件告知：{body}"
    );
    assert_eq!(mock.generate_count(), 1, "中途断流不得重放（PLAN.md 8.1）");
}

#[tokio::test]
async fn missing_credentials_yields_401_before_any_upstream_call() {
    let mock = MockUpstream::start([MockResponse::StreamSuccess {
        text: "never".into(),
    }])
    .await;
    let no_keys: cc_server::KeyResolver = Arc::new(|_: &AccountSlot| None);
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), no_keys).unwrap());

    let (status, body) = post_chat(
        state,
        json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["error"]["code"], "invalid_api_key");
    assert_eq!(mock.generate_count(), 0, "没有凭据时不该打上游");
}

#[tokio::test]
async fn observer_records_each_request() {
    let mock = MockUpstream::start([MockResponse::StreamSuccess { text: "ok".into() }]).await;
    let records: Arc<std::sync::Mutex<Vec<RequestRecord>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = records.clone();
    let observer: cc_server::RequestObserver = Arc::new(move |rec| sink.lock().unwrap().push(rec));
    let state = Arc::new(
        ProxyState::new(config_for(mock.base_url()), two_slots(), resolver())
            .unwrap()
            .with_observer(observer),
    );

    let (status, _) = post_chat(
        state,
        json!({"model": "deepseek/deepseek-v4-flash", "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let recs = records.lock().unwrap();
    assert_eq!(recs.len(), 1, "每次请求都应上报一条记录");
    assert_eq!(recs[0].account_id, "default");
    assert_eq!(recs[0].model, "deepseek/deepseek-v4-flash");
    assert_eq!(recs[0].protocol, "cli");
    assert!(recs[0].stream);
    assert_eq!(recs[0].error_code, None);
}

#[tokio::test]
async fn model_routing_rule_directs_the_request_to_a_specific_account() {
    // 把某个模型固定路由到第二个账号
    let mock = MockUpstream::start([MockResponse::StreamSuccess {
        text: "routed".into(),
    }])
    .await;
    let rules = vec![cc_server::ModelAccountRule {
        models: vec!["special/model".into()],
        account: "second".into(),
    }];
    let state = Arc::new(
        ProxyState::new(config_for(mock.base_url()), two_slots(), resolver())
            .unwrap()
            .with_rules(rules),
    );

    let (status, _) = post_chat(
        state,
        json!({"model": "special/model", "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        mock.generate_requests()[0].bearer_token(),
        Some("key-second"),
        "命中路由规则的模型应使用指定账号"
    );
}

#[tokio::test]
async fn health_and_models_endpoints_are_reachable() {
    let mock = MockUpstream::start([MockResponse::StreamSuccess { text: "x".into() }]).await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let app = router(state.clone());
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let app = router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["object"], "list");
}

// ---------------------------------------------------------------- Anthropic 面

/// 从 Anthropic SSE 文本里收集 (event, data) 对。
fn anthropic_events(sse: &str) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let mut event: Option<String> = None;
    for line in sse.lines() {
        if let Some(name) = line.strip_prefix("event: ") {
            event = Some(name.to_string());
        } else if let Some(payload) = line.strip_prefix("data: ") {
            if let Ok(value) = serde_json::from_str::<Value>(payload) {
                out.push((event.clone().unwrap_or_default(), value));
            }
        }
    }
    out
}

#[tokio::test]
async fn anthropic_stream_emits_the_required_event_sequence() {
    let mock = MockUpstream::start([MockResponse::StreamSuccess {
        text: "hello claude".into(),
    }])
    .await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let (status, body) = post_to(
        state,
        "/v1/messages",
        json!({
            "model": "m",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let events = anthropic_events(&body);
    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();

    // Anthropic 的流式事件顺序是硬性的：SDK 会按状态机校验
    assert_eq!(
        names.first().copied(),
        Some("message_start"),
        "序列必须以 message_start 开头"
    );
    assert!(names.contains(&"content_block_start"));
    assert!(names.contains(&"content_block_delta"));
    assert!(names.contains(&"content_block_stop"));
    assert!(names.contains(&"message_delta"));
    assert_eq!(
        names.last().copied(),
        Some("message_stop"),
        "序列必须以 message_stop 结束"
    );

    // 文本内容要真的出现
    let text: String = events
        .iter()
        .filter_map(|(_, data)| data["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "hello claude");
}

#[tokio::test]
async fn anthropic_non_stream_returns_a_message_object() {
    let mock = MockUpstream::start([MockResponse::StreamSuccess {
        text: "buffered".into(),
    }])
    .await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let (status, body) = post_to(
        state,
        "/v1/messages",
        json!({
            "model": "m",
            "max_tokens": 100,
            "messages": [{"role": "user", "content": "hi"}]
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_str(&body).expect("非流式应返回 JSON");
    assert_eq!(value["type"], "message");
    assert_eq!(value["role"], "assistant");
    assert_eq!(value["content"][0]["type"], "text");
    assert_eq!(value["content"][0]["text"], "buffered");
    assert!(value["stop_reason"].is_string());
}

#[tokio::test]
async fn anthropic_errors_use_the_anthropic_envelope() {
    // 无凭据：错误信封必须是 Anthropic 形状，否则 Claude SDK 解析失败
    let mock = MockUpstream::start([MockResponse::StreamSuccess {
        text: "never".into(),
    }])
    .await;
    let no_keys: cc_server::KeyResolver = Arc::new(|_: &AccountSlot| None);
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), no_keys).unwrap());

    let (status, body) = post_to(
        state,
        "/v1/messages",
        json!({"model": "m", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        value["type"], "error",
        "Anthropic 的错误信封顶层是 type=error"
    );
    assert!(value["error"]["type"].is_string());
    assert!(value["error"]["message"].is_string());
    assert!(
        value.get("error").and_then(|e| e.get("code")).is_none()
            || value["error"]["code"].is_null(),
        "Anthropic 信封没有 OpenAI 的 code 字段"
    );
}

#[tokio::test]
async fn anthropic_request_also_goes_through_the_account_pool() {
    // 401 换号逻辑对两种协议都必须生效
    let mock = MockUpstream::start([
        MockResponse::HttpError {
            status: 401,
            body: r#"{"error":{"code":"x"}}"#.into(),
        },
        MockResponse::StreamSuccess {
            text: "from second".into(),
        },
    ])
    .await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let (status, body) = post_to(
        state,
        "/v1/messages",
        json!({"model": "m", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}], "stream": true}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(mock.generate_count(), 2, "Anthropic 面同样要走账号轮换");
    let text: String = anthropic_events(&body)
        .iter()
        .filter_map(|(_, data)| data["delta"]["text"].as_str())
        .collect();
    assert_eq!(text, "from second");
}

#[tokio::test]
async fn anthropic_system_and_tools_survive_the_conversion() {
    // Anthropic 的 system 是顶层字段（不是消息）；tools 用 input_schema。
    // 转换后必须能在上游请求体里看到它们。
    let mock = MockUpstream::start([MockResponse::StreamSuccess { text: "ok".into() }]).await;
    let state =
        Arc::new(ProxyState::new(config_for(mock.base_url()), two_slots(), resolver()).unwrap());

    let (status, _) = post_to(
        state,
        "/v1/messages",
        json!({
            "model": "m",
            "max_tokens": 100,
            "system": [{"type": "text", "text": "be terse"}],
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "name": "read_file",
                "description": "read",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}
            }],
            "stream": true
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let body: Value = serde_json::from_str(&mock.generate_requests()[0].body).unwrap();
    assert_eq!(
        body["params"]["system"], "be terse",
        "Anthropic 顶层 system 必须折成上游的字符串 system（PROTOCOL.md #1）"
    );
    assert_eq!(body["params"]["tools"][0]["name"], "read_file");
    assert_eq!(body["params"]["tools"][0]["input_schema"]["type"], "object");
}
