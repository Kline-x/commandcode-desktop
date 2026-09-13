//! 上游事件 → OpenAI SSE 的转换（响应方向）。
//!
//! 对应 third_party/proxy.mjs 的 createSseTranslator / makeChunk。
//!
//! 设计要点：这是一个**有状态**的转换器（[ChunkBuilder]），因为 OpenAI 的流式协议
//! 有若干隐含不变式必须跨事件维持：
//!
//! - 首个 chunk 必须带 \`role: "assistant"\`，之后的 chunk 不带；
//! - \`finish_reason\` 只在最后一个 chunk 出现，且**只出现一次**；
//! - \`usage\` 只挂在最后那个 chunk 上；
//! - tool_calls 的 index 从 0 递增，且每个调用必须给出完整的 id/name/arguments
//!   （上游是一次性给出完整 tool-call，不需要像原生 OpenAI 那样分片拼接）。
//!
//! 另有两条来自上游的**行为约束**（不是格式问题）：
//! - 上游的 \`error\` 事件**不产生** finish_reason chunk：若在此处发出 finish，
//!   后续真正的 finish(tool_calls) 会被下游 agent loop 忽略——它们通常在首个
//!   finish_reason 处停止。错误由连接层抛出（见 [crate::proxy]）。
//! - usage 的 output 为 0 时整体清零（防伪账），见 [crate::sse::Usage::normalized]。

use serde_json::{json, Value};

use crate::sse::{map_finish_reason, UpstreamEvent, Usage};

/// 生成一个 OpenAI 的流式 chunk。
fn make_chunk(
    id: &str,
    created: i64,
    model: &str,
    delta: Value,
    finish_reason: Option<&str>,
    usage: Option<Value>,
) -> Value {
    let mut chunk = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }],
    });
    if let Some(u) = usage {
        chunk["usage"] = u;
    }
    chunk
}

/// 把内部用量转成 OpenAI 的 usage 形状。
///
/// \`prompt_tokens\` 用上游的 inputTokens（**已含缓存部分**），
/// 缓存命中单独放在 \`prompt_tokens_details.cached_tokens\`——下游若把两者相加
/// 会得到约两倍，这是 OpenAI 与 Anthropic 语义相反的地方之一（PROTOCOL.md #7）。
fn openai_usage(usage: Usage) -> Value {
    let usage = usage.normalized();
    json!({
        "prompt_tokens": usage.input_tokens,
        "completion_tokens": usage.output_tokens,
        "total_tokens": usage.input_tokens + usage.output_tokens,
        "prompt_tokens_details": { "cached_tokens": usage.cached_input_tokens },
    })
}

/// 把上游事件流累积地转成 OpenAI SSE 事件。
#[derive(Debug)]
pub struct ChunkBuilder {
    id: String,
    created: i64,
    model: String,
    /// 是否已经发出过带 role 的首个 chunk。
    sent_role: bool,
    /// 已经发过多少内容性 chunk（用于决定是否附带 role）。
    chunk_index: usize,
    /// 上游给出的 finishReason（来自 finish-step）。
    finish_reason: Option<String>,
    /// 最近一次用量。
    usage: Option<Usage>,
    /// tool_calls 的递增下标。
    tool_call_index: usize,
}

impl ChunkBuilder {
    /// 新建一个转换器。\`created\` 由调用方注入（避免内部读系统时间）。
    pub fn new(id: impl Into<String>, created: i64, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            created,
            model: model.into(),
            sent_role: false,
            chunk_index: 0,
            finish_reason: None,
            usage: None,
            tool_call_index: 0,
        }
    }

    /// 本次生成累计的用量。
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// 构造带 role 的 delta（仅首个 chunk）。
    fn with_role(&mut self, mut delta: Value) -> Value {
        let first = self.chunk_index == 0;
        self.chunk_index += 1;
        if first {
            self.sent_role = true;
            delta["role"] = json!("assistant");
        }
        delta
    }

    /// 处理一个上游事件，返回要发给客户端的 chunk 列表。
    ///
    /// 返回空 vec 表示该事件不产生输出（例如 reasoning-end、start 等）。
    pub fn push(&mut self, event: &UpstreamEvent) -> Vec<Value> {
        match event {
            UpstreamEvent::TextDelta(text) => {
                if text.is_empty() {
                    return Vec::new();
                }
                let delta = self.with_role(json!({ "content": text }));
                vec![make_chunk(
                    &self.id,
                    self.created,
                    &self.model,
                    delta,
                    None,
                    None,
                )]
            }
            UpstreamEvent::ReasoningDelta(text) => {
                if text.is_empty() {
                    return Vec::new();
                }
                // reasoning_content 是 DeepSeek 风格的扩展字段：客户端若支持会渲染
                // 思考过程，不支持则忽略。必须透传，否则 agent 场景丢失思维链。
                let delta = self.with_role(json!({ "reasoning_content": text }));
                vec![make_chunk(
                    &self.id,
                    self.created,
                    &self.model,
                    delta,
                    None,
                    None,
                )]
            }
            UpstreamEvent::ToolCall { id, name, input } => {
                let call_id = if id.is_empty() {
                    format!("call_{}_{}", self.created, self.tool_call_index)
                } else {
                    id.clone()
                };
                let arguments = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                let index = self.tool_call_index;
                self.tool_call_index += 1;
                let delta = self.with_role(json!({
                    "content": Value::Null,
                    "tool_calls": [{
                        "index": index,
                        "id": call_id,
                        "type": "function",
                        "function": { "name": name, "arguments": arguments },
                    }],
                }));
                vec![make_chunk(
                    &self.id,
                    self.created,
                    &self.model,
                    delta,
                    None,
                    None,
                )]
            }
            UpstreamEvent::FinishStep {
                finish_reason,
                usage,
            } => {
                if let Some(reason) = finish_reason {
                    self.finish_reason = Some(reason.clone());
                }
                if let Some(u) = usage {
                    self.usage = Some(*u);
                }
                Vec::new()
            }
            UpstreamEvent::Finish {
                finish_reason,
                usage,
            } => {
                if let Some(u) = usage {
                    self.usage = Some(*u);
                }
                let reason = finish_reason
                    .clone()
                    .or_else(|| self.finish_reason.clone())
                    .unwrap_or_else(|| "stop".to_string());
                let usage = self.usage.map(openai_usage);
                vec![make_chunk(
                    &self.id,
                    self.created,
                    &self.model,
                    json!({}),
                    Some(map_finish_reason(&reason)),
                    usage,
                )]
            }
            // 上游的 error 事件刻意**不**产生 finish chunk：
            // 若在此发出 finish，后续真正的 finish(tool_calls) 会被下游
            // agent loop 忽略（它们在首个 finish_reason 处停止）。
            // 错误由连接层抛出，见 crate::proxy。
            UpstreamEvent::Error { .. } => Vec::new(),
            // 无用户可见内容的事件
            UpstreamEvent::TextStart
            | UpstreamEvent::TextEnd
            | UpstreamEvent::ReasoningStart
            | UpstreamEvent::ReasoningEnd
            | UpstreamEvent::Ignored => Vec::new(),
        }
    }

    /// 流意外结束（没有 finish 事件）时的收尾 chunk。
    ///
    /// 某些上游故障会直接断流而不发 finish。此时仍要给出一个 finish_reason，
    /// 否则客户端会一直等待。
    pub fn finish_without_event(&mut self) -> Option<Value> {
        let reason = self
            .finish_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string());
        let usage = self.usage.map(openai_usage);
        Some(make_chunk(
            &self.id,
            self.created,
            &self.model,
            json!({}),
            Some(map_finish_reason(&reason)),
            usage,
        ))
    }

    /// 是否已经输出过任何内容。
    pub fn saw_content(&self) -> bool {
        self.sent_role
    }
}

/// 把 JSON 序列化成一行 SSE 数据帧。
pub fn to_sse_line(value: &Value) -> String {
    format!("data: {value}\n\n")
}

/// SSE 的结束标记。
pub const SSE_DONE: &str = "data: [DONE]\n\n";

/// 构造一个完整的（非流式）OpenAI 响应体。
///
/// 上游只有流式接口，因此非流式请求由本地缓冲后用它一次性返回。
pub fn build_completion(
    id: &str,
    created: i64,
    model: &str,
    content: &str,
    reasoning: &str,
    tool_calls: &[Value],
    finish_reason: &str,
    usage: Option<Usage>,
) -> Value {
    let mut message = json!({ "role": "assistant", "content": content });
    if !reasoning.is_empty() {
        message["reasoning_content"] = json!(reasoning);
    }
    if !tool_calls.is_empty() {
        message["tool_calls"] = json!(tool_calls);
    }
    let mut body = json!({
        "id": id,
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": map_finish_reason(finish_reason),
        }],
    });
    if let Some(u) = usage {
        body["usage"] = openai_usage(u);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> ChunkBuilder {
        ChunkBuilder::new("chatcmpl-1", 1_700_000_000, "deepseek/deepseek-v4-flash")
    }

    #[test]
    fn first_chunk_carries_role_and_later_ones_do_not() {
        let mut b = builder();
        let first = b.push(&UpstreamEvent::TextDelta("hello".into()));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0]["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(first[0]["choices"][0]["delta"]["content"], "hello");

        let second = b.push(&UpstreamEvent::TextDelta(" world".into()));
        assert_eq!(
            second[0]["choices"][0]["delta"].get("role"),
            None,
            "第二个 chunk 不应再带 role"
        );
        assert_eq!(second[0]["choices"][0]["delta"]["content"], " world");
    }

    #[test]
    fn reasoning_delta_uses_reasoning_content_field() {
        let mut b = builder();
        let chunks = b.push(&UpstreamEvent::ReasoningDelta("thinking".into()));
        assert_eq!(
            chunks[0]["choices"][0]["delta"]["reasoning_content"],
            "thinking"
        );
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    }

    #[test]
    fn empty_deltas_produce_no_chunks() {
        let mut b = builder();
        assert!(b.push(&UpstreamEvent::TextDelta(String::new())).is_empty());
        assert!(b
            .push(&UpstreamEvent::ReasoningDelta(String::new()))
            .is_empty());
        // 且不应因此消耗掉「首个 chunk」的机会
        let chunks = b.push(&UpstreamEvent::TextDelta("x".into()));
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
    }

    #[test]
    fn tool_calls_get_incrementing_index_and_full_arguments() {
        let mut b = builder();
        let first = b.push(&UpstreamEvent::ToolCall {
            id: "call_a".into(),
            name: "read_file".into(),
            input: json!({"path": "/tmp/a"}),
        });
        let tc = &first[0]["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(tc["index"], 0);
        assert_eq!(tc["id"], "call_a");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "read_file");
        // 上游一次性给完整参数，不需要分片拼接
        let args: Value =
            serde_json::from_str(tc["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "/tmp/a");

        let second = b.push(&UpstreamEvent::ToolCall {
            id: "call_b".into(),
            name: "write_file".into(),
            input: json!({}),
        });
        assert_eq!(
            second[0]["choices"][0]["delta"]["tool_calls"][0]["index"],
            1
        );
    }

    #[test]
    fn tool_call_without_id_gets_a_generated_one() {
        let mut b = builder();
        let chunks = b.push(&UpstreamEvent::ToolCall {
            id: String::new(),
            name: "f".into(),
            input: json!({}),
        });
        let id = chunks[0]["choices"][0]["delta"]["tool_calls"][0]["id"]
            .as_str()
            .unwrap();
        assert!(
            !id.is_empty(),
            "缺失的 tool call id 必须补一个，否则客户端无法回传结果"
        );
    }

    #[test]
    fn finish_step_reason_is_used_when_finish_omits_it() {
        let mut b = builder();
        b.push(&UpstreamEvent::FinishStep {
            finish_reason: Some("tool-calls".into()),
            usage: None,
        });
        let chunks = b.push(&UpstreamEvent::Finish {
            finish_reason: None,
            usage: None,
        });
        assert_eq!(chunks[0]["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn finish_carries_usage() {
        let mut b = builder();
        let chunks = b.push(&UpstreamEvent::Finish {
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 3,
            }),
        });
        let usage = &chunks[0]["usage"];
        assert_eq!(usage["prompt_tokens"], 10);
        assert_eq!(usage["completion_tokens"], 5);
        assert_eq!(usage["total_tokens"], 15);
        assert_eq!(usage["prompt_tokens_details"]["cached_tokens"], 3);
    }

    #[test]
    fn zero_output_tokens_zeroes_usage() {
        let mut b = builder();
        let chunks = b.push(&UpstreamEvent::Finish {
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                input_tokens: 999,
                output_tokens: 0,
                cached_input_tokens: 0,
            }),
        });
        assert_eq!(
            chunks[0]["usage"]["prompt_tokens"], 0,
            "防伪账：output=0 时整体清零"
        );
    }

    #[test]
    fn error_event_does_not_emit_finish_reason() {
        let mut b = builder();
        let chunks = b.push(&UpstreamEvent::Error {
            message: "boom".into(),
            code: None,
        });
        assert!(
            chunks.is_empty(),
            "error 事件不得发出 finish chunk，否则会吞掉后续的 tool_calls 收尾"
        );
    }

    #[test]
    fn structural_events_produce_nothing() {
        let mut b = builder();
        for ev in [
            UpstreamEvent::TextStart,
            UpstreamEvent::TextEnd,
            UpstreamEvent::ReasoningStart,
            UpstreamEvent::ReasoningEnd,
            UpstreamEvent::Ignored,
        ] {
            assert!(b.push(&ev).is_empty(), "{ev:?} 不应产生输出");
        }
    }

    #[test]
    fn finish_without_event_closes_the_stream() {
        let mut b = builder();
        b.push(&UpstreamEvent::TextDelta("partial".into()));
        let chunk = b.finish_without_event().unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn sse_framing_matches_the_wire_format() {
        let line = to_sse_line(&json!({"a": 1}));
        assert!(line.starts_with("data: "));
        assert!(line.ends_with("\n\n"));
        assert_eq!(SSE_DONE, "data: [DONE]\n\n");
    }

    #[test]
    fn non_streaming_completion_shape() {
        let body = build_completion(
            "chatcmpl-1",
            1_700_000_000,
            "m",
            "hi",
            "thinking",
            &[],
            "stop",
            Some(Usage {
                input_tokens: 1,
                output_tokens: 2,
                cached_input_tokens: 0,
            }),
        );
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["choices"][0]["message"]["content"], "hi");
        assert_eq!(
            body["choices"][0]["message"]["reasoning_content"],
            "thinking"
        );
        assert_eq!(body["choices"][0]["finish_reason"], "stop");
        assert_eq!(body["usage"]["total_tokens"], 3);
    }
}
