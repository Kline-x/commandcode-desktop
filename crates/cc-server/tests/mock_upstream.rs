//! [MockUpstream] 自身的端到端测试：用真实 HTTP 客户端打真实服务器。
//!
//! 这里**不测被测代理**，只测 mock 这个测试基础设施本身是否可信——如果 mock
//! 的 NDJSON 形状或请求记录是错的，建在它上面的所有链路测试都会变成假绿。
//!
//! 之所以能直接 use cc_server::mock_upstream：Cargo.toml 里有一行自引用
//! dev-dependency（cc-server = { path = ".", features = ["mock"] }），
//! 它只在测试目标里打开 mock feature。理由见该模块的文档注释。

use std::time::Duration;

use cc_server::mock_upstream::{
    MockBehavior, MockResponse, MockUpstream, DEFAULT_EVENT_GAP_MS, DEFAULT_STREAM_TEXT,
    GENERATE_PATH, MODELS_PATH, UPSTREAM_REQUEST_HEADERS,
};
use serde_json::Value;

/// 测试用的请求体：形状对齐 docs/PROTOCOL.md 第 2 节（只保留断言需要的字段）。
fn generate_body() -> Value {
    serde_json::json!({
        "config": { "workingDir": "/tmp" },
        "memory": Value::Null,
        "taste": Value::Null,
        "skills": Value::Null,
        "params": {
            "model": "deepseek/deepseek-v4-pro",
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": "hi" }] }],
            "system": " ",
            "max_tokens": 64000,
            "temperature": 0.3,
            "stream": true
        },
        "threadId": "thread-test"
    })
}

/// 测试用的请求头：覆盖 docs/PROTOCOL.md 第 2 节清单的一部分。
fn generate_headers() -> [(&'static str, &'static str); 3] {
    [
        ("Authorization", "Bearer user_test-abcd"),
        ("x-command-code-version", "1.53.1"),
        ("x-cli-environment", "production"),
    ]
}

/// 打一次 /alpha/generate，返回响应。
async fn post_generate(upstream: &MockUpstream) -> reqwest::Response {
    let url = format!("{}{GENERATE_PATH}", upstream.base_url());
    let mut request = reqwest::Client::new().post(url).json(&generate_body());
    for (name, value) in generate_headers() {
        request = request.header(name, value);
    }
    request
        .send()
        .await
        .expect("mock 上游已在监听，请求不应在传输层失败")
}

#[tokio::test]
async fn stream_success_emits_text_deltas_then_finish() {
    let upstream = MockUpstream::start(vec![MockResponse::StreamSuccess {
        text: DEFAULT_STREAM_TEXT.to_string(),
    }])
    .await;

    let response = post_generate(&upstream).await;
    assert_eq!(response.status(), 200, "正常流应以 200 返回");
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/x-ndjson"),
        "上游 /alpha/generate 是 NDJSON，不是标准 SSE（PROTOCOL.md 第 5 节）"
    );

    // 关键：body 必须**先完整读出来**再解析——请求记录里的 body 是空的，
    // 事件在响应侧。
    let body = response.text().await.expect("正常流应能完整读完");
    let events: Vec<Value> = body
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("每一行都应是合法 JSON"))
        .collect();

    assert!(!events.is_empty(), "至少要有 text-delta 与 finish 事件");

    let deltas: Vec<&str> = events
        .iter()
        .filter(|event| event["type"] == "text-delta")
        .map(|event| event["text"].as_str().expect("text-delta 必须带 text 字段"))
        .collect();
    assert!(
        deltas.len() >= 2,
        "文本应被切成多个 text-delta，才能覆盖按块解帧的路径；实际 {}",
        deltas.len()
    );
    assert_eq!(
        deltas.concat(),
        DEFAULT_STREAM_TEXT,
        "逐块拼接必须还原出完整文本，否则分块逻辑丢了字符"
    );

    let last = events.last().expect("事件列表非空");
    assert_eq!(last["type"], "finish", "最后一个事件必须是 finish");
    assert_eq!(last["finishReason"], "stop");
    assert_eq!(
        last["totalUsage"]["outputTokens"], 5,
        "finish 应带上游形状的 usage（camelCase）"
    );

    // 裸 NDJSON 不得出现 SSE 的 data: 前缀或事件分隔空行。
    assert!(
        !body.contains("data:"),
        "真实上游不发 data: 前缀，mock 也不该发，否则测不出代理的分帧 bug"
    );
}

#[tokio::test]
async fn http_error_returns_the_configured_status_and_body() {
    let upstream = MockUpstream::start(vec![MockResponse::HttpError {
        status: 429,
        body: r#"{"error":{"code":"rate_limited","message":"too many requests"}}"#.to_string(),
    }])
    .await;

    let response = post_generate(&upstream).await;
    assert_eq!(response.status(), 429, "应原样返回配置的状态码");
    let body = response.text().await.expect("错误响应应能读完");
    assert!(
        body.contains("rate_limited"),
        "错误 body 必须原样回传：代理的 mapCcError 要读 error.code，实际 {body}"
    );
}

#[tokio::test]
async fn upgrade_required_is_a_403_with_the_code_in_both_places() {
    let upstream = MockUpstream::start(vec![MockResponse::UpgradeRequired]).await;

    let response = post_generate(&upstream).await;
    assert_eq!(response.status(), 403, "upgrade_required 是 403");
    let body = response.text().await.expect("错误响应应能读完");
    let parsed: Value = serde_json::from_str(&body).expect("body 应是合法 JSON");
    assert_eq!(
        parsed["error"]["code"], "upgrade_required",
        "error.code 必须带 upgrade_required：error.rs 的 is_upgrade_required 首选读它"
    );
    assert!(
        parsed["error"]["message"]
            .as_str()
            .expect("message 应是字符串")
            .contains("upgrade_required"),
        "message 里也要有 upgrade_required，覆盖只读 body 的兼容路径"
    );
}

#[tokio::test]
async fn requests_records_method_path_headers_and_json_body() {
    let upstream = MockUpstream::start(vec![MockResponse::StreamSuccess {
        text: "ok".to_string(),
    }])
    .await;

    let response = post_generate(&upstream).await;
    // 必须把 body 读完，否则请求处理可能尚未走到记录之后的收尾逻辑。
    let _ = response.text().await.expect("响应应能读完");

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "一次请求只应记录一条");
    let request = &requests[0];

    assert_eq!(request.method, reqwest::Method::POST);
    assert_eq!(request.path, GENERATE_PATH);
    assert_eq!(
        request.header("authorization"),
        Some("Bearer user_test-abcd"),
        "Authorization 应被记录；轮换测试要靠它断言换号"
    );
    assert_eq!(
        request.bearer_token(),
        Some("user_test-abcd"),
        "bearer_token 应剥掉 Bearer 前缀"
    );
    assert_eq!(
        request.header("Authorization"),
        Some("Bearer user_test-abcd"),
        "请求头查询应不区分大小写"
    );
    assert_eq!(request.header("x-command-code-version"), Some("1.53.1"));
    assert_eq!(request.sequence, 0, "第一条请求的序号是 0");

    let json = request.json().expect("请求体应能解析为 JSON");
    assert_eq!(
        json["params"]["system"], " ",
        "PROTOCOL.md #3：无 system 时必须发一个空格占位"
    );
    assert_eq!(json["params"]["messages"][0]["role"], "user");
    // 注意不能用 is_ascii_lowercase：它只认 a-z，'-' 会让它为 false。
    // 这里要的是「没有大写字母」，从而能直接与记录里归一化后的键比对。
    assert!(
        UPSTREAM_REQUEST_HEADERS
            .iter()
            .all(|name| !name.chars().any(|c| c.is_ascii_uppercase())),
        "头清单自身应小写，才能与 records 的归一化键直接比对"
    );
}

#[tokio::test]
async fn behaviors_are_consumed_by_request_sequence_and_wrap_around() {
    let upstream = MockUpstream::start(vec![
        MockResponse::HttpError {
            status: 401,
            body: r#"{"error":{"code":"unauthorized"}}"#.to_string(),
        },
        MockResponse::HttpError {
            status: 429,
            body: r#"{"error":{"code":"rate_limited"}}"#.to_string(),
        },
        MockResponse::StreamSuccess {
            text: "recovered".to_string(),
        },
    ])
    .await;

    // docs/PLAN.md 第 7 节验收：mock 连续 401 → 429 → 200，客户端无感拿到 200。
    let statuses = [
        post_generate(&upstream).await.status().as_u16(),
        post_generate(&upstream).await.status().as_u16(),
        post_generate(&upstream).await.status().as_u16(),
        // 第四次打回脚本开头，验证按序号取模的循环语义。
        post_generate(&upstream).await.status().as_u16(),
    ];
    assert_eq!(
        statuses,
        [401, 429, 200, 401],
        "行为脚本应按请求序号循环使用（第 N 次用 behaviors[N % len]）"
    );
    assert_eq!(upstream.request_count(), 4, "四次请求都应被记录");
}

#[tokio::test]
async fn repeat_behavior_holds_a_status_until_the_window_is_exhausted() {
    // MockBehavior::repeat 存在的意义就是这类场景：前三次 429，第四次才恢复。
    // 用裸 MockResponse 列表要手抄三个一模一样的响应，容易抄漏。
    let upstream = MockUpstream::start(vec![
        MockBehavior::repeat(
            MockResponse::HttpError {
                status: 429,
                body: r#"{"error":{"code":"rate_limited"}}"#.to_string(),
            },
            3,
        ),
        MockBehavior::from(MockResponse::StreamSuccess {
            text: "recovered".to_string(),
        }),
    ])
    .await;

    let statuses = [
        post_generate(&upstream).await.status().as_u16(),
        post_generate(&upstream).await.status().as_u16(),
        post_generate(&upstream).await.status().as_u16(),
        post_generate(&upstream).await.status().as_u16(),
    ];
    assert_eq!(
        statuses,
        [429, 429, 429, 200],
        "重复区间应按次数保持响应，用尽后切到下一个区间"
    );
}

#[tokio::test]
async fn unknown_paths_are_recorded_and_answered_with_404() {
    let upstream = MockUpstream::start(vec![MockResponse::StreamSuccess {
        text: "unused".to_string(),
    }])
    .await;

    let status = reqwest::Client::new()
        .post(format!("{}/alpha/unknown", upstream.base_url()))
        .json(&serde_json::json!({ "probe": true }))
        .send()
        .await
        .expect("请求应到达 mock")
        .status();
    assert_eq!(status, 404, "未实现的端点应回 404");

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1, "未命中路径的请求也必须被记录");
    assert_eq!(
        requests[0].path, "/alpha/unknown",
        "记录路径才能定位「代理打到了哪」"
    );
}

#[tokio::test]
async fn models_endpoint_lists_provider_models() {
    let upstream = MockUpstream::start(vec![MockResponse::StreamSuccess {
        text: "unused".to_string(),
    }])
    .await;

    let response = reqwest::Client::new()
        .get(format!("{}{MODELS_PATH}", upstream.base_url()))
        .send()
        .await
        .expect("模型目录请求应到达 mock");
    assert_eq!(response.status(), 200);

    let body: Value = response.json().await.expect("目录响应应是合法 JSON");
    let ids: Vec<&str> = body["data"]
        .as_array()
        .expect("目录形状是 { object, data: [...] }")
        .iter()
        .map(|model| model["id"].as_str().expect("每个模型必须有 id"))
        .collect();
    assert!(
        ids.contains(&"deepseek/deepseek-v4-pro"),
        "目录应包含上游模型 id，实际 {ids:?}"
    );

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, MODELS_PATH);
}

#[tokio::test]
async fn broken_mid_stream_delivers_partial_text_then_transport_error() {
    let upstream = MockUpstream::start(vec![MockResponse::BrokenMidStream {
        text: DEFAULT_STREAM_TEXT.to_string(),
    }])
    .await;

    let response = post_generate(&upstream).await;
    assert_eq!(
        response.status(),
        200,
        "断流发生在响应头之后：状态已经是 200（PLAN.md 第 8.1 节）"
    );

    let outcome = response.text().await;
    assert!(
        outcome.is_err(),
        "断流必须让客户端看到传输错误，而不是「意外的正常结束」；实际得到 {outcome:?}"
    );
}

#[tokio::test]
async fn hang_sends_headers_but_no_events() {
    let mut upstream = MockUpstream::start(vec![MockResponse::Hang]).await;

    let response = post_generate(&upstream).await;
    assert_eq!(response.status(), 200, "空闲夹具也是先回 200 头再保持静默");

    // PROTOCOL.md #4：空闲只计 read() 等待，所以「等不到任何字节」就是它要模拟的形态。
    let read = tokio::time::timeout(
        Duration::from_millis(DEFAULT_EVENT_GAP_MS * 20),
        response.text(),
    )
    .await;
    assert!(
        read.is_err(),
        "Hang 行为在超时窗口内不应产生任何事件，否则空闲超时用例失去意义"
    );

    // 显式关闭，顺带验证「放行 Hang 的连接」这条收尾路径不会把测试挂死。
    upstream.shutdown().await;
}

#[tokio::test]
async fn received_at_ms_comes_from_the_injected_clock() {
    const NOW_MS: i64 = 1_700_000_000_000;
    let upstream = MockUpstream::start_at(
        vec![MockResponse::StreamSuccess {
            text: "x".to_string(),
        }],
        NOW_MS,
    )
    .await;

    let _ = post_generate(&upstream).await.text().await;
    let requests = upstream.requests();
    assert_eq!(
        requests[0].received_at_ms, NOW_MS,
        "时间必须由参数注入（STYLE.md 2.3），mock 内部不得读系统时间"
    );
}
