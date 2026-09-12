//! 上游 NDJSON / SSE 流的解析器与事件类型。
//!
//! 上游 /alpha/generate 返回的是 **NDJSON**（每行一个 JSON），不是标准 SSE；
//! 而 /provider/v1/chat/completions 返回标准 SSE。两者共用本模块的行切分逻辑。
//!
//! 关键约束（见 docs/PROTOCOL.md 第 5 节）：同一时刻**最多一个 text 块和一个
//! reasoning 块**打开，互相切换时先关闭前一个。

use serde_json::Value;

/// 上游 /alpha/generate 的一个流事件。
///
/// 用「已归一化的枚举」而非裸 Value，是为了让未知事件在**一处**被显式忽略，
/// 而不是散落在各处的 if let。
#[derive(Debug, Clone, PartialEq)]
pub enum UpstreamEvent {
    /// 文本增量。
    TextDelta(String),
    /// 正文块开始（无用户可见内容，仅需关闭 reasoning 块）。
    TextStart,
    /// 正文块结束。
    TextEnd,
    /// 思考开始。
    ReasoningStart,
    /// 思考增量。
    ReasoningDelta(String),
    /// 思考结束。
    ReasoningEnd,
    /// 工具调用（上游一次性给出完整参数）。
    ToolCall {
        /// 上游的工具调用 id。
        id: String,
        /// 工具名。
        name: String,
        /// 参数（已是 JSON 值）。
        input: Value,
    },
    /// 一步结束，可能带 finishReason 与 usage。
    FinishStep {
        /// 上游的结束原因。
        finish_reason: Option<String>,
        /// 该步的用量。
        usage: Option<Usage>,
    },
    /// 整个生成结束。
    Finish {
        /// 结束原因。
        finish_reason: Option<String>,
        /// 总用量。
        usage: Option<Usage>,
    },
    /// 上游报错。
    Error {
        /// 错误消息。
        message: String,
        /// 上游的错误码。
        code: Option<String>,
    },
    /// 已知但无用户可见内容的事件（tool-input-delta、provider-metadata 等）。
    Ignored,
}

/// 上游用量统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// 输入 token。
    pub input_tokens: u64,
    /// 输出 token。
    pub output_tokens: u64,
    /// 命中缓存的输入 token。
    pub cached_input_tokens: u64,
}

impl Usage {
    /// 从上游的 usage JSON 解析。字段缺失一律按 0 处理（上游字段会漂移）。
    ///
    /// 注意 inputTokens 在上游是**含缓存部分的总数**（对照 docs/PROTOCOL.md #7：
    /// Anthropic 侧的 cache_read 是独立增量，两者语义相反）。
    pub fn from_json(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let num = |key: &str| obj.get(key).and_then(Value::as_u64).unwrap_or(0);
        Some(Self {
            input_tokens: num("inputTokens"),
            output_tokens: num("outputTokens"),
            cached_input_tokens: num("cachedInputTokens"),
        })
    }

    /// 输出为 0 时清零全部计数：防止上游的假计费（与上游实现一致的防伪账处理）。
    pub fn normalized(self) -> Self {
        if self.output_tokens == 0 {
            Self::default()
        } else {
            self
        }
    }
}

/// 把一行原始文本解析为一个事件。
///
/// 返回 None 表示该行不含事件（空行、SSE 注释、[DONE]、无法解析的 JSON，
/// 或缺少 type 字段）。**无法解析的行被静默跳过**——上游偶发会插入非 JSON 行，
/// 为此中断整条流是不值得的。
pub fn parse_line(line: &str) -> Option<UpstreamEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" || trimmed.starts_with(':') {
        return None;
    }
    // 兼容标准 SSE 的 data: 前缀
    let payload = trimmed
        .strip_prefix("data:")
        .map(str::trim)
        .unwrap_or(trimmed);
    if payload.is_empty() || payload == "[DONE]" {
        return None;
    }
    let value: Value = serde_json::from_str(payload).ok()?;
    event_from_json(&value)
}

/// 把一个已解析的 JSON 值转成事件。
pub fn event_from_json(value: &Value) -> Option<UpstreamEvent> {
    let event_type = value.get("type")?.as_str()?;
    let text = || {
        value
            .get("text")
            .or_else(|| value.get("delta"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Some(match event_type {
        "text-delta" => UpstreamEvent::TextDelta(text()),
        "text-start" => UpstreamEvent::TextStart,
        "text-end" => UpstreamEvent::TextEnd,
        "reasoning-start" => UpstreamEvent::ReasoningStart,
        "reasoning-delta" => UpstreamEvent::ReasoningDelta(text()),
        "reasoning-end" => UpstreamEvent::ReasoningEnd,
        "tool-call" => UpstreamEvent::ToolCall {
            id: value
                .get("toolCallId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            input: value
                .get("input")
                .or_else(|| value.get("args"))
                .or_else(|| value.get("arguments"))
                .cloned()
                .unwrap_or_else(|| Value::Object(Default::default())),
        },
        "finish-step" => UpstreamEvent::FinishStep {
            finish_reason: value
                .get("finishReason")
                .and_then(Value::as_str)
                .map(str::to_string),
            usage: value.get("usage").and_then(Usage::from_json),
        },
        "finish" => UpstreamEvent::Finish {
            finish_reason: value
                .get("finishReason")
                .and_then(Value::as_str)
                .map(str::to_string),
            usage: value
                .get("totalUsage")
                .or_else(|| value.get("usage"))
                .and_then(Usage::from_json),
        },
        "error" => UpstreamEvent::Error {
            message: value
                .get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("Unknown error")
                .to_string(),
            code: value
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(Value::as_str)
                .map(str::to_string),
        },
        // 已知但无用户可见内容的事件；未知事件同样忽略（调用方自行决定是否记日志）
        _ => UpstreamEvent::Ignored,
    })
}

/// 把上游的结束原因映射到 OpenAI 的取值。
pub fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "tool-calls" | "tool_calls" | "tool-use" | "tool_use" => "tool_calls",
        "length" | "max-tokens" | "max_tokens" => "length",
        "content-filter" | "content_filter" => "content_filter",
        _ => "stop",
    }
}

/// 行切分器：把字节流按换行切成完整行，最后一行可能没有换行符。
///
/// 单独抽出并暴露 take_remainder，是为了让调用方在流结束时**不丢**最后一行的
/// 内容——上游的最后一个 finish 事件常常没有结尾换行。
#[derive(Debug, Default)]
pub struct LineBuffer {
    buffer: String,
}

impl LineBuffer {
    /// 追加一段解码后的文本，返回其中完整的行。
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buffer.push_str(chunk);
        let mut lines = Vec::new();
        while let Some(idx) = self.buffer.find(NEWLINE) {
            let line: String = self.buffer.drain(..=idx).collect();
            lines.push(line.trim_end_matches(['\r', '\n']).to_string());
        }
        lines
    }

    /// 取出并清空尾部残留（流结束时的最后一行）。
    pub fn take_remainder(&mut self) -> Option<String> {
        if self.buffer.trim().is_empty() {
            self.buffer.clear();
            return None;
        }
        Some(std::mem::take(&mut self.buffer))
    }
}

/// 换行符常量：写成常量避免在源码里出现裸转义，减少阅读与编辑时的歧义。
const NEWLINE: char = '\n';

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ignores_noise_lines() {
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
        assert!(parse_line("[DONE]").is_none());
        assert!(parse_line(": keep-alive").is_none());
        assert!(
            parse_line("not json").is_none(),
            "无法解析的行应被跳过而不是中断流"
        );
        assert!(parse_line(r#"{"no_type":1}"#).is_none());
    }

    #[test]
    fn accepts_both_ndjson_and_sse_framing() {
        let ndjson = parse_line(r#"{"type":"text-delta","text":"hi"}"#);
        let sse = parse_line(r#"data: {"type":"text-delta","text":"hi"}"#);
        assert_eq!(ndjson, Some(UpstreamEvent::TextDelta("hi".into())));
        assert_eq!(ndjson, sse, "NDJSON 与 SSE 两种承载应解析出同一事件");
    }

    #[test]
    fn text_delta_falls_back_to_delta_field() {
        // 上游不同版本用过 text / delta 两个字段名
        assert_eq!(
            event_from_json(&json!({"type": "text-delta", "delta": "x"})),
            Some(UpstreamEvent::TextDelta("x".into()))
        );
    }

    #[test]
    fn tool_call_reads_any_of_the_three_field_names() {
        for key in ["input", "args", "arguments"] {
            let ev = event_from_json(&json!({
                "type": "tool-call",
                "toolCallId": "c1",
                "toolName": "read",
                key: {"path": "/tmp"}
            }));
            match ev {
                Some(UpstreamEvent::ToolCall { id, name, input }) => {
                    assert_eq!(id, "c1");
                    assert_eq!(name, "read");
                    assert_eq!(input["path"], "/tmp");
                }
                other => panic!("expected ToolCall, got {other:?}"),
            }
        }
    }

    #[test]
    fn unknown_events_are_ignored_not_fatal() {
        assert_eq!(
            event_from_json(&json!({"type": "brand-new-event"})),
            Some(UpstreamEvent::Ignored)
        );
    }

    #[test]
    fn usage_missing_fields_default_to_zero() {
        let u = Usage::from_json(&json!({"inputTokens": 10})).unwrap();
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cached_input_tokens, 0);
    }

    #[test]
    fn zero_output_tokens_zeroes_the_whole_usage() {
        // 防伪账：上游只在 outputTokens>0 时才真的处理了请求
        let u = Usage {
            input_tokens: 100,
            output_tokens: 0,
            cached_input_tokens: 50,
        };
        assert_eq!(u.normalized(), Usage::default());
        let ok = Usage {
            input_tokens: 100,
            output_tokens: 5,
            cached_input_tokens: 50,
        };
        assert_eq!(ok.normalized(), ok);
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(map_finish_reason("tool-calls"), "tool_calls");
        assert_eq!(map_finish_reason("tool_use"), "tool_calls");
        assert_eq!(map_finish_reason("max-tokens"), "length");
        assert_eq!(map_finish_reason("stop"), "stop");
        assert_eq!(map_finish_reason("whatever"), "stop");
    }

    #[test]
    fn line_buffer_keeps_partial_line() {
        let mut buf = LineBuffer::default();
        assert!(
            buf.push(r#"{"type":"text-del"#).is_empty(),
            "不完整行不应产出"
        );
        let lines = buf.push("ta\",\"text\":\"a\"}\n");
        assert_eq!(lines.len(), 1);
        assert_eq!(
            parse_line(&lines[0]),
            Some(UpstreamEvent::TextDelta("a".into()))
        );
    }

    #[test]
    fn line_buffer_exposes_remainder_without_trailing_newline() {
        let mut buf = LineBuffer::default();
        buf.push(r#"{"type":"finish"}"#);
        assert_eq!(
            buf.take_remainder().as_deref().map(parse_line),
            Some(Some(UpstreamEvent::Finish {
                finish_reason: None,
                usage: None
            })),
            "流结束时最后一行的 finish 事件不能丢"
        );
        assert!(buf.take_remainder().is_none(), "取走后应清空");
    }

    #[test]
    fn line_buffer_handles_crlf_and_empty_remainder() {
        let mut buf = LineBuffer::default();
        let lines = buf.push("a\r\nb\n");
        assert_eq!(lines, vec!["a".to_string(), "b".to_string()]);
        assert!(buf.take_remainder().is_none());
    }
}
