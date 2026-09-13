//! OpenAI Responses API（`/v1/responses`）↔ 内部 Chat 格式的双向转换。
//!
//! 供 Codex 等使用 Responses 协议的客户端接入。代理作为无状态转换层：
//! 把 input 翻译成内部 Chat 格式，复用同一套 CC 转发管线。
//! 不支持 `previous_response_id` / `store`（需要服务端保存会话，与无状态定位冲突），
//! 收到直接 400 报错，避免静默降级成错误答案。
//!
//! 核心数据流：
//!
//!   Responses 请求 --convert_responses_to_chat--> OpenAI Chat 请求 --convert.rs--> /alpha/generate
//!   上游事件流 --ResponsesSseBuilder--> Responses SSE（流式）
//!   上游事件流 --build_responses_object--> Responses JSON（非流式）

use serde_json::{json, Value};
use uuid::Uuid;

use crate::error::CcError;
use crate::sse::{UpstreamEvent, Usage};

/// 生成指定前缀的 24 位随机 ID（形如 `resp_...` / `msg_...` / `fc_...` / `rs_...`）。
pub fn new_responses_id(prefix: &str) -> String {
    let raw = Uuid::new_v4().simple().to_string();
    let suffix = if raw.len() >= 24 { &raw[..24] } else { &raw };
    format!("{prefix}{suffix}")
}

/// 提取 content 中的文本（支持纯字符串或 content block 数组）。
pub fn responses_text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(arr) => {
            let mut buf = String::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    buf.push_str(text);
                }
            }
            buf
        }
        _ => String::new(),
    }
}

/// 提取 reasoning 项中的推理文本。
pub fn responses_reasoning_of(item: &Value) -> String {
    if let Some(summary) = item.get("summary").and_then(Value::as_array) {
        let mut buf = String::new();
        for p in summary {
            if let Some(text) = p.get("text").and_then(Value::as_str) {
                buf.push_str(text);
            }
        }
        if !buf.is_empty() {
            return buf;
        }
    }
    if let Some(content) = item.get("content").and_then(Value::as_array) {
        let mut buf = String::new();
        for p in content {
            if let Some(text) = p.get("text").and_then(Value::as_str) {
                buf.push_str(text);
            }
        }
        if !buf.is_empty() {
            return buf;
        }
    }
    item.get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 内部暂存的 assistant 消息结构。
#[derive(Default)]
struct PendingAssistant {
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Vec<Value>,
}

impl PendingAssistant {
    fn is_empty(&self) -> bool {
        self.content.is_none() && self.reasoning_content.is_none() && self.tool_calls.is_empty()
    }

    fn into_message(self) -> Option<Value> {
        if self.is_empty() {
            return None;
        }
        let mut map = serde_json::Map::new();
        map.insert("role".into(), json!("assistant"));
        if let Some(c) = self.content {
            map.insert("content".into(), json!(c));
        } else {
            map.insert("content".into(), Value::Null);
        }
        if let Some(r) = self.reasoning_content {
            map.insert("reasoning_content".into(), json!(r));
        }
        if !self.tool_calls.is_empty() {
            map.insert("tool_calls".into(), json!(self.tool_calls));
        }
        Some(Value::Object(map))
    }
}

/// 把 Responses API 请求转换为标准 OpenAI Chat Completions 请求体。
pub fn convert_responses_to_chat(resp_req: &Value) -> Result<Value, CcError> {
    if let Some(prev) = resp_req.get("previous_response_id") {
        if !prev.is_null() {
            return Err(CcError::Protocol(
                "previous_response_id is not supported (this proxy is stateless); send the full input each turn".into(),
            ));
        }
    }
    if resp_req.get("store").and_then(Value::as_bool) == Some(true) {
        return Err(CcError::Protocol(
            "store=true is not supported in stateless proxy".into(),
        ));
    }

    let mut messages: Vec<Value> = Vec::new();

    if let Some(inst) = resp_req.get("instructions") {
        if !inst.is_null() {
            let sys = responses_text_of(inst);
            if !sys.is_empty() {
                messages.push(json!({
                    "role": "system",
                    "content": sys
                }));
            }
        }
    }

    let mut pending = PendingAssistant::default();
    let flush_pending = |messages: &mut Vec<Value>, pending: &mut PendingAssistant| {
        if let Some(msg) = std::mem::take(pending).into_message() {
            messages.push(msg);
        }
    };

    if let Some(input) = resp_req.get("input") {
        match input {
            Value::String(s) => {
                messages.push(json!({
                    "role": "user",
                    "content": s
                }));
            }
            Value::Array(items) => {
                for item in items {
                    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
                    match item_type {
                        "reasoning" => {
                            let text = responses_reasoning_of(item);
                            if !text.is_empty() {
                                pending.reasoning_content = Some(text);
                            }
                        }
                        "message" => {
                            let role = item.get("role").and_then(Value::as_str).unwrap_or("user");
                            let content_val = item.get("content").unwrap_or(&Value::Null);
                            let text = responses_text_of(content_val);
                            if role == "assistant" {
                                if !text.is_empty() {
                                    pending.content = Some(text);
                                }
                            } else if role == "system" || role == "developer" {
                                flush_pending(&mut messages, &mut pending);
                                messages.push(json!({
                                    "role": "system",
                                    "content": text
                                }));
                            } else {
                                flush_pending(&mut messages, &mut pending);
                                messages.push(json!({
                                    "role": "user",
                                    "content": text
                                }));
                            }
                        }
                        "function_call" => {
                            let call_id = item
                                .get("call_id")
                                .or_else(|| item.get("id"))
                                .and_then(Value::as_str)
                                .map(str::to_string)
                                .unwrap_or_else(|| new_responses_id("call_"));
                            let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                            let arguments = match item.get("arguments") {
                                Some(Value::String(s)) => s.clone(),
                                Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
                                None => "{}".into(),
                            };
                            pending.tool_calls.push(json!({
                                "id": call_id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": arguments
                                }
                            }));
                        }
                        "function_call_output" => {
                            flush_pending(&mut messages, &mut pending);
                            let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                            let content = match item.get("output") {
                                Some(Value::String(s)) => s.clone(),
                                Some(v) => serde_json::to_string(v).unwrap_or_default(),
                                None => String::new(),
                            };
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": call_id,
                                "content": content
                            }));
                        }
                        _ => {
                            // 容错：许多客户端（包括 OpenAI Responses 官方 SDK 或直接 curl）
                            // 发送的 input 数组项不带 type: "message"，但包含 role 与 content，或者为纯字符串
                            if let Some(s) = item.as_str() {
                                flush_pending(&mut messages, &mut pending);
                                messages.push(json!({
                                    "role": "user",
                                    "content": s
                                }));
                            } else if let Some(role) = item.get("role").and_then(Value::as_str) {
                                let content_val = item.get("content").unwrap_or(&Value::Null);
                                let text = responses_text_of(content_val);
                                if role == "assistant" {
                                    if !text.is_empty() {
                                        pending.content = Some(text);
                                    }
                                } else if role == "system" || role == "developer" {
                                    flush_pending(&mut messages, &mut pending);
                                    messages.push(json!({
                                        "role": "system",
                                        "content": text
                                    }));
                                } else {
                                    flush_pending(&mut messages, &mut pending);
                                    messages.push(json!({
                                        "role": "user",
                                        "content": text
                                    }));
                                }
                            } else if let Some(content_val) = item.get("content") {
                                let text = responses_text_of(content_val);
                                if !text.is_empty() {
                                    flush_pending(&mut messages, &mut pending);
                                    messages.push(json!({
                                        "role": "user",
                                        "content": text
                                    }));
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    flush_pending(&mut messages, &mut pending);

    if messages.is_empty() {
        return Err(CcError::Protocol("input is required".into()));
    }

    let model = resp_req
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("deepseek/deepseek-v4-flash")
        .to_string();
    let stream = resp_req
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let mut out = serde_json::Map::new();
    out.insert("model".into(), json!(model));
    out.insert("messages".into(), json!(messages));
    out.insert("stream".into(), json!(stream));

    if let Some(tools) = resp_req.get("tools").and_then(Value::as_array) {
        let converted_tools: Vec<Value> = tools
            .iter()
            .filter_map(|t| {
                let name = t
                    .get("name")
                    .and_then(Value::as_str)
                    .or_else(|| t.pointer("/function/name").and_then(Value::as_str))?;
                let desc = t
                    .get("description")
                    .and_then(Value::as_str)
                    .or_else(|| t.pointer("/function/description").and_then(Value::as_str))
                    .unwrap_or("");
                let params = t
                    .get("parameters")
                    .or_else(|| t.pointer("/function/parameters"))
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
                Some(json!({
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": desc,
                        "parameters": params
                    }
                }))
            })
            .collect();
        if !converted_tools.is_empty() {
            out.insert("tools".into(), json!(converted_tools));
        }
    }

    if let Some(tc) = resp_req.get("tool_choice") {
        if let Some(s) = tc.as_str() {
            out.insert("tool_choice".into(), json!(s));
        } else if let Some(obj) = tc.as_object() {
            if let Some(name) = obj.get("name").and_then(Value::as_str) {
                out.insert(
                    "tool_choice".into(),
                    json!({ "type": "function", "function": { "name": name } }),
                );
            }
        }
    }

    if let Some(max_tok) = resp_req.get("max_output_tokens") {
        out.insert("max_tokens".into(), max_tok.clone());
    }
    if let Some(temp) = resp_req.get("temperature") {
        out.insert("temperature".into(), temp.clone());
    }
    if let Some(top_p) = resp_req.get("top_p") {
        out.insert("top_p".into(), top_p.clone());
    }
    if let Some(ptc) = resp_req.get("parallel_tool_calls") {
        out.insert("parallel_tool_calls".into(), ptc.clone());
    }
    if let Some(effort) = resp_req
        .pointer("/reasoning/effort")
        .and_then(Value::as_str)
    {
        out.insert("reasoning_effort".into(), json!(effort));
    }

    Ok(Value::Object(out))
}

/// 构造 Responses 用量对象。
pub fn build_responses_usage(usage: Option<&Usage>) -> Value {
    let in_tok = usage.map(|u| u.input_tokens).unwrap_or(0);
    let out_tok = usage.map(|u| u.output_tokens).unwrap_or(0);
    let cached_tok = usage.map(|u| u.cached_input_tokens).unwrap_or(0);
    json!({
        "input_tokens": in_tok,
        "input_tokens_details": {
            "cached_tokens": cached_tok,
            "cache_write_tokens": 0
        },
        "output_tokens": out_tok,
        "output_tokens_details": {
            "reasoning_tokens": 0
        },
        "total_tokens": in_tok + out_tok
    })
}

/// 构造非流式 Responses 响应对象。
#[allow(clippy::too_many_arguments)]
pub fn build_responses_object(
    response_id: &str,
    model: &str,
    created: i64,
    content: &str,
    reasoning: &str,
    tool_calls: &[Value],
    finish_reason: &str,
    usage: Option<&Usage>,
) -> Value {
    let truncated = finish_reason == "length";
    let now_ts = crate::time::now_epoch_ms() / 1000;

    let mut output: Vec<Value> = Vec::new();
    if !reasoning.is_empty() {
        output.push(json!({
            "type": "reasoning",
            "id": new_responses_id("rs_"),
            "summary": [{
                "type": "summary_text",
                "text": reasoning
            }]
        }));
    }
    if !content.is_empty() {
        output.push(json!({
            "type": "message",
            "id": new_responses_id("msg_"),
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": content,
                "annotations": []
            }]
        }));
    }
    for tc in tool_calls {
        let call_id = tc.get("id").and_then(Value::as_str).unwrap_or("");
        let name = tc
            .pointer("/function/name")
            .and_then(Value::as_str)
            .unwrap_or("");
        let raw_args = tc.pointer("/function/arguments");
        let arguments = match raw_args {
            Some(Value::String(s)) => s.clone(),
            Some(v) => serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
            None => "{}".into(),
        };
        output.push(json!({
            "type": "function_call",
            "id": new_responses_id("fc_"),
            "call_id": call_id,
            "name": name,
            "arguments": arguments,
            "status": "completed"
        }));
    }

    json!({
        "id": response_id,
        "object": "response",
        "created_at": created,
        "status": if truncated { "incomplete" } else { "completed" },
        "completed_at": now_ts,
        "error": null,
        "incomplete_details": if truncated { json!({ "reason": "max_output_tokens" }) } else { Value::Null },
        "input": [],
        "instructions": null,
        "max_output_tokens": null,
        "model": model,
        "output": output,
        "output_text": content,
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "reasoning": null,
        "store": false,
        "temperature": 1,
        "text": { "format": { "type": "text" } },
        "tool_choice": "auto",
        "tools": [],
        "top_p": 1,
        "truncation": "disabled",
        "usage": build_responses_usage(usage),
        "user": null,
        "metadata": {}
    })
}

/// 正在流式输出的项目状态。
enum CurrentKind {
    Message,
    Reasoning,
    FunctionCall,
}

struct CurrentStreamItem {
    kind: CurrentKind,
    index: usize,
    id: String,
    name: String,
    call_id: String,
    text_buf: String,
}

/// 有状态的 Responses SSE 翻译器。
pub struct ResponsesSseBuilder {
    response_id: String,
    created: i64,
    model: String,
    seq: u64,
    created_sent: bool,
    output_index: usize,
    current: Option<CurrentStreamItem>,
    done_items: Vec<Value>,
    text_acc: String,
    finish_reason: Option<String>,
    usage: Option<Usage>,
}

impl ResponsesSseBuilder {
    /// 构造一个新的 Responses 流翻译器。
    pub fn new(response_id: String, created: i64, model: String) -> Self {
        Self {
            response_id,
            created,
            model,
            seq: 0,
            created_sent: false,
            output_index: 0,
            current: None,
            done_items: Vec::new(),
            text_acc: String::new(),
            finish_reason: None,
            usage: None,
        }
    }

    fn sse_event(&mut self, event_type: &str, mut data: serde_json::Map<String, Value>) -> String {
        data.insert("type".into(), json!(event_type));
        data.insert("sequence_number".into(), json!(self.seq));
        self.seq += 1;
        let serialized = serde_json::to_string(&data).unwrap_or_default();
        format!("event: {event_type}\ndata: {serialized}\n\n")
    }

    fn base_response(&self, status: &str, output: Option<Vec<Value>>) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "created_at": self.created,
            "status": status,
            "output": output.unwrap_or_default(),
            "output_text": self.text_acc,
            "model": self.model,
            "error": null,
            "incomplete_details": null,
            "parallel_tool_calls": true,
            "previous_response_id": null,
            "store": false,
            "tools": [],
            "metadata": {}
        })
    }

    fn start_response(&mut self) -> Vec<String> {
        self.created_sent = true;
        let mut out = Vec::new();
        let mut m1 = serde_json::Map::new();
        m1.insert("response".into(), self.base_response("in_progress", None));
        out.push(self.sse_event("response.created", m1));

        let mut m2 = serde_json::Map::new();
        m2.insert("response".into(), self.base_response("in_progress", None));
        out.push(self.sse_event("response.in_progress", m2));
        out
    }

    fn close_current(&mut self) -> Vec<String> {
        let Some(curr) = self.current.take() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        match curr.kind {
            CurrentKind::Message => {
                let mut m1 = serde_json::Map::new();
                m1.insert("item_id".into(), json!(curr.id));
                m1.insert("output_index".into(), json!(curr.index));
                m1.insert("content_index".into(), json!(0));
                m1.insert("text".into(), json!(curr.text_buf));
                m1.insert("logprobs".into(), json!([]));
                out.push(self.sse_event("response.output_text.done", m1));

                let mut m2 = serde_json::Map::new();
                m2.insert("item_id".into(), json!(curr.id));
                m2.insert("output_index".into(), json!(curr.index));
                m2.insert("content_index".into(), json!(0));
                m2.insert(
                    "part".into(),
                    json!({
                        "type": "output_text",
                        "text": curr.text_buf,
                        "annotations": []
                    }),
                );
                out.push(self.sse_event("response.content_part.done", m2));

                let item = json!({
                    "type": "message",
                    "id": curr.id,
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": curr.text_buf,
                        "annotations": []
                    }]
                });
                let mut m3 = serde_json::Map::new();
                m3.insert("output_index".into(), json!(curr.index));
                m3.insert("item".into(), item.clone());
                out.push(self.sse_event("response.output_item.done", m3));
                self.done_items.push(item);
            }
            CurrentKind::Reasoning => {
                let mut m1 = serde_json::Map::new();
                m1.insert("item_id".into(), json!(curr.id));
                m1.insert("output_index".into(), json!(curr.index));
                m1.insert("summary_index".into(), json!(0));
                m1.insert("text".into(), json!(curr.text_buf));
                out.push(self.sse_event("response.reasoning_summary_text.done", m1));

                let mut m2 = serde_json::Map::new();
                m2.insert("item_id".into(), json!(curr.id));
                m2.insert("output_index".into(), json!(curr.index));
                m2.insert("summary_index".into(), json!(0));
                m2.insert(
                    "part".into(),
                    json!({
                        "type": "summary_text",
                        "text": curr.text_buf
                    }),
                );
                out.push(self.sse_event("response.reasoning_summary_part.done", m2));

                let item = json!({
                    "type": "reasoning",
                    "id": curr.id,
                    "status": "completed",
                    "summary": [{
                        "type": "summary_text",
                        "text": curr.text_buf
                    }]
                });
                let mut m3 = serde_json::Map::new();
                m3.insert("output_index".into(), json!(curr.index));
                m3.insert("item".into(), item.clone());
                out.push(self.sse_event("response.output_item.done", m3));
                self.done_items.push(item);
            }
            CurrentKind::FunctionCall => {
                let mut m1 = serde_json::Map::new();
                m1.insert("item_id".into(), json!(curr.id));
                m1.insert("output_index".into(), json!(curr.index));
                m1.insert("arguments".into(), json!(curr.text_buf));
                out.push(self.sse_event("response.function_call_arguments.done", m1));

                let item = json!({
                    "type": "function_call",
                    "id": curr.id,
                    "call_id": curr.call_id,
                    "name": curr.name,
                    "arguments": curr.text_buf,
                    "status": "completed"
                });
                let mut m2 = serde_json::Map::new();
                m2.insert("output_index".into(), json!(curr.index));
                m2.insert("item".into(), item.clone());
                out.push(self.sse_event("response.output_item.done", m2));
                self.done_items.push(item);
            }
        }
        out
    }

    /// 接收一个上游事件，产出 0 到多行 SSE 响应。
    pub fn push(&mut self, event: &UpstreamEvent) -> Vec<String> {
        let mut out = Vec::new();
        match event {
            UpstreamEvent::TextDelta(text) => {
                if !self.created_sent {
                    out.extend(self.start_response());
                }
                let needs_open = match &self.current {
                    Some(c) => !matches!(c.kind, CurrentKind::Message),
                    None => true,
                };
                if needs_open {
                    out.extend(self.close_current());
                    let id = new_responses_id("msg_");
                    let index = self.output_index;
                    self.output_index += 1;
                    let item = json!({
                        "type": "message",
                        "id": id,
                        "status": "in_progress",
                        "role": "assistant",
                        "content": []
                    });
                    let mut m1 = serde_json::Map::new();
                    m1.insert("output_index".into(), json!(index));
                    m1.insert("item".into(), item);
                    out.push(self.sse_event("response.output_item.added", m1));

                    let mut m2 = serde_json::Map::new();
                    m2.insert("item_id".into(), json!(id));
                    m2.insert("output_index".into(), json!(index));
                    m2.insert("content_index".into(), json!(0));
                    m2.insert(
                        "part".into(),
                        json!({ "type": "output_text", "text": "", "annotations": [] }),
                    );
                    out.push(self.sse_event("response.content_part.added", m2));

                    self.current = Some(CurrentStreamItem {
                        kind: CurrentKind::Message,
                        index,
                        id,
                        name: String::new(),
                        call_id: String::new(),
                        text_buf: String::new(),
                    });
                }

                if let Some(curr) = &mut self.current {
                    curr.text_buf.push_str(text);
                    self.text_acc.push_str(text);
                    let mut m = serde_json::Map::new();
                    m.insert("item_id".into(), json!(curr.id));
                    m.insert("output_index".into(), json!(curr.index));
                    m.insert("content_index".into(), json!(0));
                    m.insert("delta".into(), json!(text));
                    m.insert("logprobs".into(), json!([]));
                    out.push(self.sse_event("response.output_text.delta", m));
                }
            }
            UpstreamEvent::ReasoningDelta(text) => {
                if !self.created_sent {
                    out.extend(self.start_response());
                }
                let needs_open = match &self.current {
                    Some(c) => !matches!(c.kind, CurrentKind::Reasoning),
                    None => true,
                };
                if needs_open {
                    out.extend(self.close_current());
                    let id = new_responses_id("rs_");
                    let index = self.output_index;
                    self.output_index += 1;
                    let item = json!({
                        "type": "reasoning",
                        "id": id,
                        "status": "in_progress",
                        "summary": []
                    });
                    let mut m1 = serde_json::Map::new();
                    m1.insert("output_index".into(), json!(index));
                    m1.insert("item".into(), item);
                    out.push(self.sse_event("response.output_item.added", m1));

                    let mut m2 = serde_json::Map::new();
                    m2.insert("item_id".into(), json!(id));
                    m2.insert("output_index".into(), json!(index));
                    m2.insert("summary_index".into(), json!(0));
                    m2.insert("part".into(), json!({ "type": "summary_text", "text": "" }));
                    out.push(self.sse_event("response.reasoning_summary_part.added", m2));

                    self.current = Some(CurrentStreamItem {
                        kind: CurrentKind::Reasoning,
                        index,
                        id,
                        name: String::new(),
                        call_id: String::new(),
                        text_buf: String::new(),
                    });
                }

                if let Some(curr) = &mut self.current {
                    curr.text_buf.push_str(text);
                    let mut m = serde_json::Map::new();
                    m.insert("item_id".into(), json!(curr.id));
                    m.insert("output_index".into(), json!(curr.index));
                    m.insert("summary_index".into(), json!(0));
                    m.insert("delta".into(), json!(text));
                    out.push(self.sse_event("response.reasoning_summary_text.delta", m));
                }
            }
            UpstreamEvent::ToolCall { id, name, input } => {
                if !self.created_sent {
                    out.extend(self.start_response());
                }
                out.extend(self.close_current());
                let fc_id = new_responses_id("fc_");
                let index = self.output_index;
                self.output_index += 1;
                let args_str = match input {
                    Value::String(s) => s.clone(),
                    v => serde_json::to_string(v).unwrap_or_else(|_| "{}".into()),
                };

                let item = json!({
                    "type": "function_call",
                    "id": fc_id,
                    "call_id": id,
                    "name": name,
                    "arguments": "",
                    "status": "in_progress"
                });
                let mut m1 = serde_json::Map::new();
                m1.insert("output_index".into(), json!(index));
                m1.insert("item".into(), item);
                out.push(self.sse_event("response.output_item.added", m1));

                let mut m2 = serde_json::Map::new();
                m2.insert("item_id".into(), json!(fc_id));
                m2.insert("output_index".into(), json!(index));
                m2.insert("delta".into(), json!(args_str));
                out.push(self.sse_event("response.function_call_arguments.delta", m2));

                self.current = Some(CurrentStreamItem {
                    kind: CurrentKind::FunctionCall,
                    index,
                    id: fc_id,
                    name: name.clone(),
                    call_id: id.clone(),
                    text_buf: args_str,
                });
            }
            UpstreamEvent::FinishStep {
                finish_reason,
                usage,
            } => {
                if let Some(fr) = finish_reason {
                    self.finish_reason = Some(fr.clone());
                }
                if let Some(u) = usage {
                    self.usage = Some(*u);
                }
            }
            UpstreamEvent::Finish {
                finish_reason,
                usage,
            } => {
                if let Some(fr) = finish_reason {
                    self.finish_reason = Some(fr.clone());
                }
                if let Some(u) = usage {
                    self.usage = Some(*u);
                }
            }
            _ => {}
        }
        out
    }

    /// 正常结束生成，产出收尾的 completed 或 incomplete 事件。
    pub fn finish(&mut self) -> Vec<String> {
        if !self.created_sent {
            return Vec::new();
        }
        let mut out = self.close_current();
        let truncated = self.finish_reason.as_deref() == Some("length");
        let event_name = if truncated {
            "response.incomplete"
        } else {
            "response.completed"
        };
        let mut resp = self.base_response(
            if truncated { "incomplete" } else { "completed" },
            Some(self.done_items.clone()),
        );
        if let Some(obj) = resp.as_object_mut() {
            obj.insert("output_text".into(), json!(self.text_acc));
            obj.insert(
                "incomplete_details".into(),
                if truncated {
                    json!({ "reason": "max_output_tokens" })
                } else {
                    Value::Null
                },
            );
            obj.insert("usage".into(), build_responses_usage(self.usage.as_ref()));
        }
        let mut m = serde_json::Map::new();
        m.insert("response".into(), resp);
        out.push(self.sse_event(event_name, m));
        out
    }

    /// 生成中途发生错误，产出 `response.failed`。
    pub fn fail(&mut self, message: &str) -> Vec<String> {
        if !self.created_sent {
            return Vec::new();
        }
        let mut out = self.close_current();
        let mut resp = self.base_response("failed", Some(self.done_items.clone()));
        if let Some(obj) = resp.as_object_mut() {
            obj.insert(
                "error".into(),
                json!({ "code": "upstream_error", "message": message }),
            );
        }
        let mut m = serde_json::Map::new();
        m.insert("response".into(), resp);
        out.push(self.sse_event("response.failed", m));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_previous_response_id_and_store() {
        let req1 = json!({
            "previous_response_id": "resp_123",
            "input": "hello"
        });
        assert!(convert_responses_to_chat(&req1).is_err());

        let req2 = json!({
            "store": true,
            "input": "hello"
        });
        assert!(convert_responses_to_chat(&req2).is_err());
    }

    #[test]
    fn convert_simple_string_input() {
        let req = json!({
            "model": "gpt-5",
            "instructions": "Be concise",
            "input": "Hello world",
            "stream": true,
            "max_output_tokens": 100
        });
        let converted = convert_responses_to_chat(&req).expect("convert ok");
        assert_eq!(converted["model"], "gpt-5");
        assert_eq!(converted["stream"], true);
        assert_eq!(converted["max_tokens"], 100);
        let msgs = converted["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "Be concise");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "Hello world");
    }

    #[test]
    fn convert_items_array_with_reasoning_and_tools() {
        let req = json!({
            "input": [
                {
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": "let me think" }]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "I will run a tool" }]
                },
                {
                    "type": "function_call",
                    "call_id": "call_abc",
                    "name": "calc",
                    "arguments": "{\"x\":1}"
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_abc",
                    "output": "2"
                }
            ]
        });
        let converted = convert_responses_to_chat(&req).expect("convert ok");
        let msgs = converted["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 2);
        // assistant message has reasoning, content, and tool_calls merged together
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], "I will run a tool");
        assert_eq!(msgs[0]["reasoning_content"], "let me think");
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_abc");
        // tool message
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "call_abc");
        assert_eq!(msgs[1]["content"], "2");
    }

    #[test]
    fn build_responses_object_non_streaming() {
        let tc = vec![json!({
            "id": "call_1",
            "function": {
                "name": "lookup",
                "arguments": "{\"q\":\"weather\"}"
            }
        })];
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 2,
        };
        let res = build_responses_object(
            "resp_123",
            "model-x",
            1700000000,
            "it is sunny",
            "thinking hard",
            &tc,
            "stop",
            Some(&usage),
        );
        assert_eq!(res["id"], "resp_123");
        assert_eq!(res["status"], "completed");
        assert_eq!(res["output_text"], "it is sunny");
        let output = res["output"].as_array().expect("output array");
        assert_eq!(output.len(), 3); // reasoning, message, function_call
        assert_eq!(output[0]["type"], "reasoning");
        assert_eq!(output[1]["type"], "message");
        assert_eq!(output[2]["type"], "function_call");
        assert_eq!(res["usage"]["input_tokens"], 10);
        assert_eq!(res["usage"]["output_tokens"], 5);
        assert_eq!(res["usage"]["total_tokens"], 15);
    }

    #[test]
    fn responses_sse_builder_flow() {
        let mut builder =
            ResponsesSseBuilder::new("resp_test".into(), 1700000000, "deepseek".into());

        // push text
        let events1 = builder.push(&UpstreamEvent::TextDelta("hello ".into()));
        assert!(events1.iter().any(|s| s.contains("response.created")));
        assert!(events1.iter().any(|s| s.contains("response.in_progress")));
        assert!(events1
            .iter()
            .any(|s| s.contains("response.output_item.added")));
        assert!(events1
            .iter()
            .any(|s| s.contains("response.output_text.delta")));

        let events2 = builder.push(&UpstreamEvent::TextDelta("world".into()));
        assert!(events2
            .iter()
            .any(|s| s.contains("response.output_text.delta")));

        // finish
        let events_end = builder.finish();
        assert!(events_end
            .iter()
            .any(|s| s.contains("response.output_text.done")));
        assert!(events_end
            .iter()
            .any(|s| s.contains("response.content_part.done")));
        assert!(events_end
            .iter()
            .any(|s| s.contains("response.output_item.done")));
        assert!(events_end.iter().any(|s| s.contains("response.completed")));
    }
}
