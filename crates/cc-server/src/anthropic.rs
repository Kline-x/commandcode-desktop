//! Anthropic Messages API ↔ 上游（OpenAI Chat Completions）的双向转换。
//!
//! 本模块是**纯函数 / 纯状态机层**：不读系统时间、不碰磁盘、不发网络请求，全部输入
//! 经参数注入（STYLE.md 2.3），因此每个分支都能被确定性单测覆盖。
//!
//! 参考实现是 third_party/proxy.mjs 的 handleMessages（约 1804 行起）、
//! convertAnthropicToOpenAI（1424 行）、buildAnthropicResponse（1389 行）、
//! createAnthropicSseTranslator（1573 行）、mapAnthropicStopReason（1367 行）、
//! fakeThinkingSignature（1383 行）与 anthropicInputTokens（773 行）。
//! 参考实现里带 issue 编号或「真机验证」标注的坑，在下方逐条保留了说明，
//! **不要**在没有重新抓包的情况下「优化」掉。
//!
//! 协议依据：docs/PROTOCOL.md 第 2 节（请求形状）、第 4 节（硬约束坑位表，尤其
//! #1 system 恒为字符串、#2 reasoning 次序、#12 不支持 stop）；docs/PLAN.md
//! 第 7 节坑位 7（input_tokens 减法）与坑位 8（thinking 签名伪造）。
//!
//! 数据流：
//!
//!   Anthropic 请求 --anthropic_to_openai--> OpenAI 请求 --convert.rs--> /alpha/generate
//!   上游事件流 --AnthropicSseBuilder--> Anthropic SSE（流式）
//!   上游事件流 --build_anthropic_response--> Anthropic JSON（非流式）
//!
//! 为什么请求方向不直接产出 /alpha/generate 请求体：CLI 通道与 Provider 通道共用同一份
//! 转换结果，且 [crate::convert] 已经承载了全部真机验证过的坑位（system 占位、reasoning
//! 次序、input_schema 根校验、max_tokens 夹紧）。在这里重新实现一遍会让两处漂移。

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::error::CcError;
use crate::sse::{UpstreamEvent, Usage};

/// base64 source 缺少 media_type 时的兜底 MIME 类型。
///
/// Anthropic 的 image block 语法上要求 media_type，但中转层拼错字段时并不罕见。
/// 用 image/png 而不是 application/octet-stream：后者会让上游视觉模型把它当二进制附件
/// 拒掉，而 png 是当前最常见的截图/粘贴图片格式，猜错的代价最小。
const DEFAULT_IMAGE_MEDIA_TYPE: &str = "image/png";

/// thinking 文本为空时的签名种子（与参考实现一致，保证签名永远非空）。
const EMPTY_THINKING_SIGNATURE_SEED: &str = "dsh-proxy-thinking";

/// 伪造签名的第一个字节。
///
/// Anthropic 的 thinking signature 是 protobuf 信封的 base64；Claude Code 只做浅校验
/// （base64 且以 'E' / 'R' 开头）。0x12 = field 2, wire type 2（length-delimited），
/// 编码后的首字符因此恒为 'E'。见 docs/PLAN.md 第 7 节坑位 8。
const SIGNATURE_FIELD_TAG: u8 = 0x12;

/// 估算输出 token 时每位字符折算的 token 数（参考实现用 4）。
const ESTIMATED_CHARS_PER_TOKEN: u64 = 4;

/// 每个工具调用在估算里折算的 token 数（参考实现用 20）。
const ESTIMATED_TOKENS_PER_TOOL_CALL: u64 = 20;

/// base64 标准字母表（RFC 4648，含 padding）。
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 把上游 / OpenAI 的结束原因映射为 Anthropic 的取值。
///
/// Anthropic 只认 end_turn / max_tokens / stop_sequence / tool_use 四个值（外加 pause_turn
/// 等实验值，本项目不产生）。上游可能给 OpenAI 风格（tool_calls / length）或
/// CC 原始风格（tool-calls / max-tokens），两种都要认；无法识别时退化为 end_turn
/// 而不是原样透传——透传未知值会让 Anthropic SDK 的枚举解析直接失败。
pub fn map_stop_reason(reason: &str) -> &'static str {
    match reason {
        "tool_calls" | "tool-calls" | "tool_use" | "tool-use" => "tool_use",
        "length" | "max-tokens" | "max_tokens" => "max_tokens",
        "stop_sequence" | "stop-sequence" => "stop_sequence",
        _ => "end_turn",
    }
}

/// 计算 Anthropic 语义下的 input_tokens：**只计非缓存部分**。
///
/// PROTOCOL.md #7 / docs/PLAN.md 第 7 节坑位 7：Anthropic 官方注释写明
/// 「Total input tokens = input_tokens + cache_creation_input_tokens + cache_read_input_tokens」，
/// 即 input_tokens 是**非缓存增量**；而 CC 的 inputTokens 是**已含缓存的总数**。
/// 直接把总数当 input_tokens 转发，下游把两者相加会得到约两倍（issue #25 真机验证）。
///
/// 参考实现优先采用上游已经算好的 inputTokenDetails.noCacheTokens，缺失时才回退到
/// 减法。本项目的 [crate::sse::Usage] 目前不承载该明细字段（Usage::from_json 只读三个
/// 顶层计数），因此这里**始终走减法**；将来若 Usage 增加该字段，应在此优先采用它。
///
/// 减法结果用 saturating_sub 夹到 0：上游在流式过程中可能先给出缓存计数、后给出总数，
/// 中途取值时缓存大于总数是可能的，返回负数会让下游把它当无符号数解析成天文数字。
pub fn input_tokens_for_anthropic(usage: Usage) -> u64 {
    usage.input_tokens.saturating_sub(usage.cached_input_tokens)
}

/// 按 thinking 文本派生一个假的签名。
///
/// 真机验证（参考实现 fakeThinkingSignature）：Anthropic 会**密码学校验** thinking 签名，
/// 第三方代理不可能签出合法值。Claude Code 的校验很浅：只要求 base64、首字符是
/// 'E'（单层信封）或 'R'（双层信封）、且载荷首字节为 0x12。按此伪造即可让 CC 正常展示
/// thinking。载荷由 thinking 文本派生，保证同一段对话里各块签名互不相同——签名重复
/// 在部分客户端上会触发异常的去重行为。
fn fake_thinking_signature(thinking_text: &str) -> String {
    // 空文本也要有签名：thinking block 一旦打开，Anthropic 客户端就要求签名字段存在。
    let seed = if thinking_text.is_empty() {
        EMPTY_THINKING_SIGNATURE_SEED
    } else {
        thinking_text
    };
    let digest = Sha256::digest(seed.as_bytes());
    let mut raw = Vec::with_capacity(digest.len() + 2);
    raw.push(SIGNATURE_FIELD_TAG);
    // sha256 恒为 32 字节，放得进 u8。
    raw.push(digest.len() as u8);
    raw.extend_from_slice(&digest);
    base64_encode(&raw)
}

/// 标准 base64 编码（带 padding）。
///
/// 自己实现而**不引入 base64 crate**：本模块只需要这一个方向、一个字母表，
/// 新增依赖会打破「单二进制约 15MB」的体积目标（STYLE.md 第 6 节）。
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        // chunks(3) 永远不产出空切片，chunk[0] 一定存在
        let first = u32::from(chunk[0]);
        let second = u32::from(chunk.get(1).copied().unwrap_or(0));
        let third = u32::from(chunk.get(2).copied().unwrap_or(0));
        let triple = (first << 16) | (second << 8) | third;
        out.push(BASE64_ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(BASE64_ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64_ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(BASE64_ALPHABET[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// 解析 OpenAI 形态的工具参数（字符串或已解析的对象）。
///
/// 与 [crate::convert] 里的同名私有函数保持一致的宽松语义：坏 JSON / 空串退化为空对象，
/// 由模型看到空参数后重试，而不是让整个响应构造失败。两处实现是有意的重复——
/// convert.rs 的 parse_tool_arguments 不公开，改动它会影响他人模块的稳定面。
fn parse_tool_arguments(arguments: &Value) -> Value {
    match arguments {
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return json!({});
            }
            match serde_json::from_str::<Value>(trimmed) {
                Ok(Value::Null) | Err(_) => json!({}),
                Ok(parsed) => parsed,
            }
        }
        Value::Null => json!({}),
        other => other.clone(),
    }
}
/// 把 Anthropic 的 image block 转成 OpenAI 的 image_url 块。
///
/// Anthropic：{type:image, source:{type:base64, media_type, data}}；
/// OpenAI：{type:image_url, image_url:{url:"data:<mime>;base64,<data>"}}。
/// source.type 为 url（网络直链）时同样接受。无法识别的 source 返回 None 由调用方丢弃——
/// 丢弃最多少一个模态，透传非法形状会让上游校验整个请求失败。
fn image_part_from_anthropic(block: &Value) -> Option<Value> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let data = source
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if data.is_empty() {
                return None;
            }
            let media_type = source
                .get("media_type")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(DEFAULT_IMAGE_MEDIA_TYPE);
            Some(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{media_type};base64,{data}") },
            }))
        }
        Some("url") => {
            let url = source
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if url.is_empty() {
                return None;
            }
            Some(json!({ "type": "image_url", "image_url": { "url": url } }))
        }
        _ => None,
    }
}

/// 把 tool_result 的 content 展平成文本（OpenAI 的 tool 消息只接受字符串）。
///
/// 与参考实现一致：字符串原样、数组取各块的 text 拼接。对象走 JSON 序列化——
/// 参考实现的 JS String(obj) 会得到 "[object Object]"，那是纯粹的调试噪音；
/// [crate::convert] 的 tool 输出同样用 JSON 序列化，这里保持一致。
fn tool_result_text(content: Option<&Value>) -> String {
    match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    // 块本身可能直接是字符串（少数中转层会拍平）
                    .or_else(|| block.as_str())
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
    }
}

/// 预扫描全部消息，建立 tool_use_id → 工具名 反查表。
///
/// 先扫一遍（而不是像参考实现那样边遍历边填）：会话恢复场景下客户端可能裁剪/重排
/// 历史，tool_result 先于配对的 tool_use 出现时，单遍填表会查不到名字并回落到
/// 「不发送 name」分支（issue #15 的现场）。预扫描让查找与顺序无关。
fn build_tool_name_index(messages: &[Value]) -> BTreeMap<String, String> {
    let mut index = BTreeMap::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(Value::Array(blocks)) = message.get("content") else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !id.is_empty() {
                index.insert(id.to_string(), name.to_string());
            }
        }
    }
    index
}

/// 转换 Anthropic 的 tool_result 块为 OpenAI 的 tool 消息。
///
/// issue #15 真机验证：OpenAI 语义里 tool 消息的 name 是可选的，会话恢复等场景下
/// tool_use_id 可能找不到对应的 assistant tool_use（历史被客户端裁剪），此时**不硬塞空
/// name**——空字符串会让上游报 "Tool result is missing"。
fn convert_tool_result(block: &Value, tool_names: &BTreeMap<String, String>) -> Value {
    let tool_use_id = block
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut message = json!({
        "role": "tool",
        "tool_call_id": tool_use_id,
        "content": tool_result_text(block.get("content")),
    });
    if let Some(name) = tool_names.get(tool_use_id) {
        message["name"] = json!(name);
    }
    message
}

/// 转换 Anthropic 的 assistant 消息。
///
/// PROTOCOL.md #2：assistant 的 reasoning 必须回传且次序为 [reasoning, text, tool-call]。
/// OpenAI 侧用 reasoning_content 字段承载（[crate::convert] 会把它提到内容块最前），
/// 因此 Anthropic 的 thinking 块在这里转成 reasoning_content——丢掉它，
/// thinking 模式下的上游会直接拒绝。
///
/// 三种内容都没有时返回 None：空 assistant 消息对上游没有任何信息量，
/// 而且 [crate::convert] 转完也会把它丢掉。
fn convert_assistant_message(message: &Value) -> Option<Value> {
    let mut text = String::new();
    let mut thinking = String::new();
    let mut tool_calls = Vec::new();
    match message.get("content") {
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        text.push_str(
                            block
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        );
                    }
                    Some("thinking") => {
                        thinking.push_str(
                            block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        );
                    }
                    Some("tool_use") => {
                        // 没有 id 的调用无法与 tool_result 配对，由 convert.rs 丢弃；
                        // 这里保留原样，避免把「过滤」策略散落在两处。
                        tool_calls.push(json!({
                            "id": block.get("id").and_then(Value::as_str).unwrap_or_default(),
                            "type": "function",
                            "function": {
                                "name": block
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default(),
                                "arguments": serde_json::to_string(
                                    &block.get("input").cloned().unwrap_or_else(|| json!({})),
                                )
                                .unwrap_or_else(|_| "{}".to_string()),
                            },
                        }));
                    }
                    // 未知块（redacted_thinking、server_tool_use 等）静默丢弃：
                    // 透传会让上游校验失败，而本项目不产生这些块。
                    _ => {}
                }
            }
        }
        // 少数中转层把 assistant content 拍平成字符串
        Some(Value::String(value)) => text.push_str(value),
        Some(Value::Null) | None => {}
        Some(_) => {}
    }
    if text.is_empty() && thinking.is_empty() && tool_calls.is_empty() {
        return None;
    }
    let mut converted = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { json!(text) },
    });
    if !thinking.is_empty() {
        converted["reasoning_content"] = json!(thinking);
    }
    if !tool_calls.is_empty() {
        converted["tool_calls"] = Value::Array(tool_calls);
    }
    Some(converted)
}
/// 转换 Anthropic 的 user 消息，可能产出**多条** OpenAI 消息。
///
/// 返回顺序与参考实现一致：先 tool 消息（OpenAI 要求 tool 消息紧跟 assistant 的
/// tool_calls），再是本条 user 消息自己的文本/图片。Anthropic 把工具结果放在
/// 「下一个 user 消息」里，若不这样重排，中间夹着的用户文本会打断 tool_call 配对。
///
/// 纯文本走**字符串** content（与参考实现一致，也是 OpenAI SDK 最常见的形状）；
/// 一旦出现图片就改走内容块数组，因为字符串承载不了 image_url。
fn convert_user_message(message: &Value, tool_names: &BTreeMap<String, String>) -> Vec<Value> {
    let mut out = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => {
            if !text.is_empty() {
                out.push(json!({ "role": "user", "content": text }));
            }
        }
        Some(Value::Array(blocks)) => {
            let mut text = String::new();
            let mut parts = Vec::new();
            let mut has_image = false;
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let value = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        // 参考实现用 += 直接拼接（不加分隔符），这里保持一致
                        text.push_str(value);
                        if !value.is_empty() {
                            parts.push(json!({ "type": "text", "text": value }));
                        }
                    }
                    Some("image") => {
                        if let Some(part) = image_part_from_anthropic(block) {
                            parts.push(part);
                            has_image = true;
                        }
                    }
                    Some("tool_result") => out.push(convert_tool_result(block, tool_names)),
                    // 未知块（document、search_result 等）丢弃
                    _ => {}
                }
            }
            if has_image {
                if !parts.is_empty() {
                    out.push(json!({ "role": "user", "content": parts }));
                }
            } else if !text.is_empty() {
                out.push(json!({ "role": "user", "content": text }));
            }
        }
        // 参考实现对非字符串 / 非数组的 content 不做处理；保持同样的宽松语义
        Some(Value::Null) | None => {}
        Some(_) => {}
    }
    out
}

/// 把 Anthropic 的 system 字段展平成字符串。
///
/// PROTOCOL.md #1：上游 params.system 恒为字符串，数组会被拒
/// （Validation error: expected string, received array at "params.system"）。
/// 参考实现对 system 用的是 join('\n')——与 [crate::convert] 的
/// extract_system_prompt 一致，**不要**「优化」成块数组。
fn flatten_system(system: Option<&Value>) -> Option<String> {
    match system {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => {
            if text.is_empty() {
                None
            } else {
                Some(text.clone())
            }
        }
        Some(Value::Array(blocks)) => {
            let joined = blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                .map(|block| {
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join("\n");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        }
        // 参考实现只处理 string / array；其他类型下的 system 视为没有
        Some(_) => None,
    }
}

/// 把 Anthropic 的 tools 转成 OpenAI 的 function 工具定义。
///
/// Anthropic 用顶层 input_schema，OpenAI 用 function.parameters。缺省 schema 必须是
/// 合法的 object 根（PROTOCOL.md #7：第三方手写 schema / MCP 会导致整轮失败），
/// 但**不**改写调用方显式给出的 schema——擅自包装会改变模型看到的参数结构。
fn convert_tools(tools: Option<&Value>) -> Result<Option<Vec<Value>>, CcError> {
    let Some(tools) = tools else {
        return Ok(None);
    };
    if tools.is_null() {
        return Ok(None);
    }
    let Value::Array(items) = tools else {
        return Err(CcError::Protocol("tools 必须是数组".to_string()));
    };
    if items.is_empty() {
        // 与参考实现一致：空数组不上送，避免上游把它当成「本轮禁用工具」
        return Ok(None);
    }
    let converted = items
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.get("name").and_then(Value::as_str).unwrap_or_default(),
                    "description": tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    "parameters": tool
                        .get("input_schema")
                        .cloned()
                        .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
                },
            })
        })
        .collect();
    Ok(Some(converted))
}

/// 把 Anthropic 的 tool_choice 转成 OpenAI 的取值。
///
/// auto → "auto"、any → "required"、tool → {type:function,function:{name}}、
/// none → "none"；type 缺省按参考实现当作 auto。指定了工具却没给名字属于非法请求，
/// 在这里显式报错而不是把 name: null 交给下游。
fn convert_tool_choice(tool_choice: Option<&Value>) -> Result<Option<Value>, CcError> {
    let Some(tool_choice) = tool_choice else {
        return Ok(None);
    };
    match tool_choice {
        Value::Null => Ok(None),
        Value::Object(object) => match object.get("type").and_then(Value::as_str) {
            None | Some("auto") => Ok(Some(json!("auto"))),
            Some("any") => Ok(Some(json!("required"))),
            Some("none") => Ok(Some(json!("none"))),
            Some("tool") => {
                let name = object
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        CcError::Protocol("tool_choice.type=tool 时必须给出 name".to_string())
                    })?;
                Ok(Some(
                    json!({ "type": "function", "function": { "name": name } }),
                ))
            }
            // 未知取值退化为 auto，而不是把请求打回：auto 是语义上最接近「没指定」的选项
            Some(_) => Ok(Some(json!("auto"))),
        },
        _ => Err(CcError::UnsupportedOption(
            "tool_choice 必须是对象".to_string(),
        )),
    }
}
/// 把 Anthropic 的 thinking 配置映射为 reasoning_effort（LiteLLM 标准映射）。
///
/// 参考实现的三档：budget >= 10000 → high、>= 5000 → medium、>= 2000 → low，
/// 更小的一律 low；adaptive 直接取 effort（缺省 medium）；disabled/none 不发送。
/// 不做值域白名单：上游档位会漂移（low..max），白名单会把新档位误挡成客户端错误。
fn reasoning_effort_from_thinking(thinking: Option<&Value>) -> Option<String> {
    let thinking = thinking?.as_object()?;
    match thinking.get("type").and_then(Value::as_str) {
        Some("disabled") | Some("none") => None,
        Some("adaptive") => Some(
            thinking
                .get("effort")
                .and_then(Value::as_str)
                .filter(|effort| !effort.is_empty())
                .unwrap_or("medium")
                .to_string(),
        ),
        _ => {
            let budget = thinking.get("budget_tokens").and_then(Value::as_u64)?;
            let effort = if budget >= 10_000 {
                "high"
            } else if budget >= 5_000 {
                "medium"
            } else {
                "low"
            };
            Some(effort.to_string())
        }
    }
}

/// 把可选的数字参数原样搬到目标对象上。
///
/// 类型不对时报 CcError::UnsupportedOption 而不是静默忽略：静默忽略会让调用方以为
/// 参数生效了，排查成本极高（与 [crate::convert] 的 optional_string 同一考虑）。
fn copy_number(
    target: &mut Map<String, Value>,
    source: &Map<String, Value>,
    key: &str,
) -> Result<(), CcError> {
    match source.get(key) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Number(number)) => {
            target.insert(key.to_string(), Value::Number(number.clone()));
            Ok(())
        }
        Some(_) => Err(CcError::UnsupportedOption(format!("{key} 必须是数字"))),
    }
}

/// 读取 Anthropic 的 max_tokens。
///
/// Anthropic 语义下 max_tokens 必填，因此转换结果里**永远**带这个字段，否则
/// [crate::convert] 会回落到它自己的默认值，两处默认值一旦漂移就难以排查。
/// 参考实现是 max_tokens || 64000（JS 的 || 把 0 与缺省同等对待），这里沿用：
/// SDK 客户端总会带上它，缺省分支只服务于手写请求。
///
/// 上限夹紧留给 [crate::convert]（MAX_GENERATE_TOKENS），避免同一个规则两处实现。
fn resolve_max_tokens(req: &Map<String, Value>) -> Result<u64, CcError> {
    let requested = match req.get("max_tokens") {
        None | Some(Value::Null) => 0,
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| CcError::UnsupportedOption("max_tokens 必须是非负整数".to_string()))?,
        Some(_) => {
            return Err(CcError::UnsupportedOption(
                "max_tokens 必须是非负整数".to_string(),
            ))
        }
    };
    Ok(if requested == 0 {
        crate::config::DEFAULT_GENERATE_MAX_TOKENS
    } else {
        requested
    })
}

/// 检查 stop_sequences 是否为空（空表示可以忽略）。
///
/// PROTOCOL.md #12：上游不支持 stop 序列，带 stop 的请求直接失败。这里显式报错而不是
/// 静默丢弃——静默丢弃会让调用方以为停止序列生效了，产出意外长度的回复。
fn ensure_no_stop_sequences(req: &Map<String, Value>) -> Result<(), CcError> {
    match req.get("stop_sequences") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Array(items)) if items.is_empty() => Ok(()),
        Some(_) => Err(CcError::UnsupportedOption(
            "stop_sequences 停止序列（上游 /alpha/generate 不支持）".to_string(),
        )),
    }
}

/// 把 Anthropic Messages 请求转换成 OpenAI Chat Completions 请求。
///
/// 产物可以直接交给 [crate::convert::build_generate_body]，由后者补齐上游环境快照并
/// 走上既有的真机验证过的转换路径。
///
/// 覆盖范围（对应参考实现 convertAnthropicToOpenAI）：
/// - system（字符串或块数组）→ 一条 role:system 消息，数组展开取 text 拼接（#1）；
/// - user 的 text / image（base64 source → data URI）/ tool_result；
/// - assistant 的 text / thinking（→ reasoning_content，见 #2）/ tool_use（→ tool_calls）；
/// - max_tokens 必填、temperature / top_p 透传、stop_sequences 非空报错（#12）；
/// - tools 的 input_schema → function.parameters；tool_choice 的取值映射；
/// - metadata.user_id → user；thinking.budget_tokens → reasoning_effort。
///
/// 非法输入一律返回 CcError::Protocol / CcError::UnsupportedOption，不 panic。
pub fn anthropic_to_openai(req: &Value) -> Result<Value, CcError> {
    let object = req
        .as_object()
        .ok_or_else(|| CcError::Protocol("Anthropic 请求体必须是 JSON 对象".to_string()))?;
    ensure_no_stop_sequences(object)?;

    let messages: &[Value] = match object.get("messages") {
        None | Some(Value::Null) => &[],
        Some(Value::Array(items)) => items,
        Some(_) => return Err(CcError::Protocol("messages 必须是数组".to_string())),
    };

    let tool_names = build_tool_name_index(messages);
    let mut converted_messages = Vec::new();
    if let Some(system) = flatten_system(object.get("system")) {
        converted_messages.push(json!({ "role": "system", "content": system }));
    }
    for message in messages {
        match message.get("role").and_then(Value::as_str) {
            Some("assistant") => {
                if let Some(converted) = convert_assistant_message(message) {
                    converted_messages.push(converted);
                }
            }
            // 未知 role 按 user 处理：与参考实现一致，也符合 #6 的归一化精神
            _ => converted_messages.extend(convert_user_message(message, &tool_names)),
        }
    }

    let mut openai = Map::new();
    if let Some(model) = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
    {
        openai.insert("model".to_string(), Value::String(model.to_string()));
    }
    openai.insert("messages".to_string(), Value::Array(converted_messages));
    openai.insert("max_tokens".to_string(), json!(resolve_max_tokens(object)?));
    openai.insert(
        "stream".to_string(),
        Value::Bool(
            object
                .get("stream")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ),
    );
    copy_number(&mut openai, object, "temperature")?;
    copy_number(&mut openai, object, "top_p")?;
    if let Some(tools) = convert_tools(object.get("tools"))? {
        openai.insert("tools".to_string(), Value::Array(tools));
    }
    if let Some(choice) = convert_tool_choice(object.get("tool_choice"))? {
        openai.insert("tool_choice".to_string(), choice);
    }
    if let Some(user) = object
        .get("metadata")
        .and_then(|metadata| metadata.get("user_id"))
        .and_then(Value::as_str)
        .filter(|user| !user.is_empty())
    {
        openai.insert("user".to_string(), Value::String(user.to_string()));
    }
    if let Some(effort) = reasoning_effort_from_thinking(object.get("thinking")) {
        openai.insert("reasoning_effort".to_string(), Value::String(effort));
    }
    Ok(Value::Object(openai))
}
/// 一个 Anthropic SSE 事件。
///
/// 单独成类型而不是直接返回字符串，是为了让调用方（以及测试）能按事件名断言顺序，
/// 而不是对拼接好的文本做子串匹配。
#[derive(Debug, Clone, PartialEq)]
pub struct AnthropicSseEvent {
    /// SSE 的 event 名（message_start / content_block_start / ...）。
    pub event: &'static str,
    /// SSE 的 data 载荷。
    pub data: Value,
}

impl AnthropicSseEvent {
    /// 序列化成线上格式的一帧：event 行 + data 行 + 空行。
    pub fn to_sse(&self) -> String {
        format!("event: {}\ndata: {}\n\n", self.event, self.data)
    }
}

/// 当前打开的内容块类型。
///
/// 工具块不在其中：它在一次 push 内完成 start → delta → stop，不需要跨事件维持状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockKind {
    /// 正文块。
    Text,
    /// 思考块（关闭前必须补一个 signature_delta）。
    Thinking,
}
/// 上游事件 → Anthropic SSE 的有状态转换器。
///
/// Anthropic 的流式事件序列是**严格**的，客户端（官方 SDK）按状态机校验：
///
///   message_start
///     content_block_start → content_block_delta* → content_block_stop   （可重复多次）
///   message_delta
///   message_stop
///
/// 因此本结构维持以下不变式：
/// - message_start **必须最先**发出，且只发一次（由 ensure_message_start 保证，
///   即便首个上游事件是 error 也先开流，与参考实现一致）；
/// - 同一时刻最多一个块打开，切换类型时先补 content_block_stop；
/// - thinking 块在 stop 之前必须补一个 signature_delta（见 [fake_thinking_signature]）；
/// - content_block_stop 全部先于 message_delta，message_stop 最后。
///
/// message_delta 的 usage 走 [input_tokens_for_anthropic] 的减法（PROTOCOL.md #7）。
#[derive(Debug)]
pub struct AnthropicSseBuilder {
    /// 消息 id（msg_...），由调用方注入。
    id: String,
    /// 模型名。
    model: String,
    /// 是否已发出 message_start。
    started: bool,
    /// 是否已收尾（finish 幂等的依据）。
    finished: bool,
    /// 下一个内容块的下标。
    next_block_index: u64,
    /// 当前打开块的下标（仅在该块打开时有效）。
    current_block_index: u64,
    /// 当前打开块的类型；None 表示没有块打开。
    current_block: Option<BlockKind>,
    /// 当前 thinking 块累积的文本（用于派生签名）。
    current_thinking_text: String,
    /// 上游给出的 Anthropic 形态结束原因。
    stop_reason: Option<&'static str>,
    /// 最近一次用量（来自 finish-step / finish）。
    usage: Option<Usage>,
}

impl AnthropicSseBuilder {
    /// 新建一个转换器。id 与 model 由调用方注入，避免内部读取系统时间或随机数。
    pub fn new(id: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            started: false,
            finished: false,
            next_block_index: 0,
            current_block_index: 0,
            current_block: None,
            current_thinking_text: String::new(),
            stop_reason: None,
            usage: None,
        }
    }

    /// 本次生成累计的用量。
    pub fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// 本次生成的输出 token（经防伪账归一化）。
    ///
    /// 调用方据此判断「零输出」：参考实现把 outputTokens == 0 的响应改判为 429，
    /// 避免下游把空响应当成成功计费。
    pub fn output_tokens(&self) -> u64 {
        self.usage.unwrap_or_default().normalized().output_tokens
    }

    /// 已确定的 Anthropic 结束原因（未收到 finish 时为 None）。
    pub fn stop_reason(&self) -> Option<&'static str> {
        self.stop_reason
    }

    /// 显式发出 message_start（调用方想立刻开流时使用）。
    ///
    /// push / finish 也会自动补发，重复调用不会产生第二个 message_start。
    pub fn message_start(&mut self) -> Vec<AnthropicSseEvent> {
        let mut out = Vec::new();
        self.ensure_message_start(&mut out);
        out
    }

    /// 处理一个上游事件，返回要下发的 Anthropic SSE 事件。
    ///
    /// 返回空 vec 表示该事件不产生输出（结构信号 / finish / error）。
    /// finish 类事件只更新状态，收尾序列由 [Self::finish] 统一发出——这样
    /// 「上游断流没发 finish」和「正常收到 finish」两条路径共用同一段收尾代码。
    pub fn push(&mut self, event: &UpstreamEvent) -> Vec<AnthropicSseEvent> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.ensure_message_start(&mut out);
        match event {
            UpstreamEvent::TextDelta(text) => {
                if text.is_empty() {
                    return out;
                }
                self.ensure_block(BlockKind::Text, &mut out);
                out.push(self.event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": self.current_block_index,
                        "delta": { "type": "text_delta", "text": text },
                    }),
                ));
            }
            UpstreamEvent::ReasoningDelta(text) => {
                // 上游 reasoning → Anthropic thinking 块（Claude Code 会把它渲染成思考过程）
                if text.is_empty() {
                    return out;
                }
                self.ensure_block(BlockKind::Thinking, &mut out);
                self.current_thinking_text.push_str(text);
                out.push(self.event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": self.current_block_index,
                        "delta": { "type": "thinking_delta", "thinking": text },
                    }),
                ));
            }
            UpstreamEvent::ToolCall { id, name, input } => {
                // 上游一次性给出完整参数；工具块不需要跨事件保持打开
                self.close_block(&mut out);
                let index = self.next_block_index;
                self.next_block_index += 1;
                let call_id = if id.is_empty() {
                    // 参考实现用随机 uuid；这里用块下标，确定性可测且同一条流内唯一
                    format!("toolu_{index}")
                } else {
                    id.clone()
                };
                let partial_json =
                    serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
                out.push(self.event(
                    "content_block_start",
                    json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": { "type": "tool_use", "id": call_id, "name": name, "input": {} },
                    }),
                ));
                out.push(self.event(
                    "content_block_delta",
                    json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "input_json_delta", "partial_json": partial_json },
                    }),
                ));
                out.push(self.event(
                    "content_block_stop",
                    json!({ "type": "content_block_stop", "index": index }),
                ));
            }
            UpstreamEvent::FinishStep {
                finish_reason,
                usage,
            }
            | UpstreamEvent::Finish {
                finish_reason,
                usage,
            } => {
                if let Some(reason) = finish_reason {
                    self.stop_reason = Some(map_stop_reason(reason));
                }
                if let Some(value) = usage {
                    self.usage = Some(*value);
                }
            }
            // 与 [crate::openai::ChunkBuilder] 一致：error 事件不在这里产生任何终止事件。
            // 若在此发出 message_delta/message_stop，后续真正的 tool_calls 收尾会被
            // 下游 agent loop 忽略（它们在首个 stop_reason 处停止）。错误由连接层抛出。
            UpstreamEvent::Error { .. } => {}
            // 无用户可见内容的事件
            UpstreamEvent::TextStart
            | UpstreamEvent::TextEnd
            | UpstreamEvent::ReasoningStart
            | UpstreamEvent::ReasoningEnd
            | UpstreamEvent::Ignored => {}
        }
        out
    }
    /// 收尾：关闭打开的块，发出 message_delta 与 message_stop。
    ///
    /// 幂等：重复调用返回空 vec。调用方即使没收到上游的 finish 事件也必须调用它，
    /// 否则客户端会一直等待（对应 [crate::openai::ChunkBuilder::finish_without_event]）。
    ///
    /// 注意：**不做**零输出改判。是否把「零输出」变成错误响应由连接层决定
    /// （参考实现在这里发 429，见 [Self::output_tokens]）。
    pub fn finish(&mut self) -> Vec<AnthropicSseEvent> {
        if self.finished {
            return Vec::new();
        }
        let mut out = Vec::new();
        self.ensure_message_start(&mut out);
        self.close_block(&mut out);
        self.finished = true;
        let usage = self.usage.unwrap_or_default().normalized();
        out.push(self.event(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": self.stop_reason.unwrap_or("end_turn") },
                "usage": {
                    "output_tokens": usage.output_tokens,
                    // 只计非缓存部分；否则下游把 input 与 cache_read 相加会得到约两倍（issue #25）
                    "input_tokens": input_tokens_for_anthropic(usage),
                    "cache_read_input_tokens": usage.cached_input_tokens,
                    // 上游 Usage 没有 cacheWrite 明细，无法区分缓存写入；恒为 0 而不是省略，
                    // 因为官方 SDK 对这个字段做数值运算，缺字段会退化成 undefined
                    "cache_creation_input_tokens": 0,
                },
            }),
        ));
        out.push(self.event("message_stop", json!({ "type": "message_stop" })));
        out
    }

    /// 构造一个事件（统一在此处写 event 名，避免各处拼错）。
    fn event(&self, name: &'static str, data: Value) -> AnthropicSseEvent {
        AnthropicSseEvent { event: name, data }
    }

    /// 保证 message_start 恰好发出一次，并且是流的第一个事件。
    fn ensure_message_start(&mut self, out: &mut Vec<AnthropicSseEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        let data = json!({
            "type": "message_start",
            "message": {
                "id": self.id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": self.model,
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            },
        });
        out.push(self.event("message_start", data));
    }

    /// 确保当前打开的是指定类型的块；与已有块类型不同时先关闭它。
    fn ensure_block(&mut self, kind: BlockKind, out: &mut Vec<AnthropicSseEvent>) {
        if self.current_block == Some(kind) {
            return;
        }
        self.close_block(out);
        let index = self.next_block_index;
        self.next_block_index += 1;
        self.current_block_index = index;
        self.current_block = Some(kind);
        if kind == BlockKind::Thinking {
            self.current_thinking_text.clear();
        }
        let content_block = match kind {
            BlockKind::Text => json!({ "type": "text", "text": "" }),
            BlockKind::Thinking => json!({ "type": "thinking", "thinking": "" }),
        };
        out.push(self.event(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": index,
                "content_block": content_block,
            }),
        ));
    }

    /// 关闭当前打开的块；thinking 块先补一个 signature_delta。
    fn close_block(&mut self, out: &mut Vec<AnthropicSseEvent>) {
        let Some(kind) = self.current_block.take() else {
            return;
        };
        let index = self.current_block_index;
        if kind == BlockKind::Thinking && !self.current_thinking_text.is_empty() {
            let signature = fake_thinking_signature(&self.current_thinking_text);
            out.push(self.event(
                "content_block_delta",
                json!({
                    "type": "content_block_delta",
                    "index": index,
                    "delta": { "type": "signature_delta", "signature": signature },
                }),
            ));
        }
        self.current_thinking_text.clear();
        out.push(self.event(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": index }),
        ));
    }
}

/// 估算输出 token（上游未回报 usage 时使用）。
///
/// 参考实现按「字符数 / 4 + 每个工具调用 20」估算，并取 max(1, ...)。
/// 绝不返回 0：下游的控制台与配额面板会把 0 展示成「本次没有输出」，
/// 而实际上明明有内容。
fn estimate_output_tokens(text: &str, thinking: &str, tool_call_count: usize) -> u64 {
    let chars = text.chars().count() as u64 + thinking.chars().count() as u64;
    let from_text = chars.div_ceil(ESTIMATED_CHARS_PER_TOKEN);
    (from_text + tool_call_count as u64 * ESTIMATED_TOKENS_PER_TOOL_CALL).max(1)
}
/// 构造一个完整的（非流式）Anthropic Messages 响应体。
///
/// 上游只有流式接口，因此非流式请求由本地缓冲后用它一次性返回。对应参考实现的
/// buildAnthropicResponse：内容块次序固定为 [thinking, text, tool_use]，
/// thinking 块必须带签名，stop_sequence 恒为 null（本项目不支持 stop 序列，#12）。
///
/// tool_calls 接受的是 OpenAI 形状（{id, type, function:{name, arguments}}，
/// 即 [crate::openai::ChunkBuilder] 与代理层收集出来的形状），arguments 既可以是
/// JSON 字符串也可以是已解析的对象。
///
/// usage.output_tokens 为 0 时按内容长度估算（防伪账：上游偶尔不回 usage，
/// 沿用 0 会让面板显示「零输出」并可能触发下游的 429 判定）。
pub fn build_anthropic_response(
    id: &str,
    model: &str,
    text: &str,
    thinking: &str,
    tool_calls: &[Value],
    finish_reason: &str,
    usage: Option<Usage>,
) -> Value {
    let mut content = Vec::new();
    if !thinking.is_empty() {
        content.push(json!({
            "type": "thinking",
            "thinking": thinking,
            "signature": fake_thinking_signature(thinking),
        }));
    }
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    for (index, call) in tool_calls.iter().enumerate() {
        let function = call.get("function");
        let name = function
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let input = function
            .and_then(|function| function.get("arguments"))
            .map(parse_tool_arguments)
            .unwrap_or_else(|| json!({}));
        let call_id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("toolu_{index}"));
        content.push(json!({
            "type": "tool_use",
            "id": call_id,
            "name": name,
            "input": input,
        }));
    }

    let normalized = usage.unwrap_or_default().normalized();
    let output_tokens = if normalized.output_tokens == 0 {
        estimate_output_tokens(text, thinking, tool_calls.len())
    } else {
        normalized.output_tokens
    };
    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": map_stop_reason(finish_reason),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": input_tokens_for_anthropic(normalized),
            "output_tokens": output_tokens,
            "cache_read_input_tokens": normalized.cached_input_tokens,
            "cache_creation_input_tokens": 0,
        },
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小的合法 Anthropic 请求，减少各用例的样板。
    fn request_with(messages: Value) -> Value {
        json!({ "model": "claude-sonnet-4-6", "max_tokens": 1024, "messages": messages })
    }

    /// 收集事件名，便于对顺序做断言。
    fn event_names(events: &[AnthropicSseEvent]) -> Vec<&'static str> {
        events.iter().map(|event| event.event).collect()
    }

    /// 从一串事件里取出所有 signature_delta 的签名。
    fn signatures(events: &[AnthropicSseEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| event.data.get("delta"))
            .filter(|delta| delta.get("type").and_then(Value::as_str) == Some("signature_delta"))
            .filter_map(|delta| delta.get("signature").and_then(Value::as_str))
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn base64_encoder_matches_the_rfc_4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                base64_encode(input.as_bytes()),
                expected,
                "{input:?} 的 base64 应与 RFC 4648 测试向量一致（签名依赖它才能被客户端解析）"
            );
        }
    }

    #[test]
    fn system_array_content_is_flattened_into_a_string() {
        let req = json!({
            "model": "m",
            "max_tokens": 100,
            "system": [
                { "type": "text", "text": "第一段" },
                { "type": "text", "text": "第二段" }
            ],
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert!(
            openai["messages"][0]["content"].is_string(),
            "PROTOCOL.md #1：system 数组必须展开成字符串，块数组会被上游拒绝"
        );
        assert_eq!(
            openai["messages"][0]["content"], "第一段\n第二段",
            "数组型 system 必须展开取 text 并用换行拼接"
        );
        assert_eq!(openai["messages"][0]["role"], "system");
    }

    #[test]
    fn string_system_is_passed_through_unchanged() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["system"] = json!("你是助手");
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(openai["messages"][0]["content"], "你是助手");
    }

    #[test]
    fn request_without_system_produces_no_system_message() {
        let req = request_with(json!([{ "role": "user", "content": "hi" }]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(
            openai["messages"].as_array().map(Vec::len),
            Some(1),
            "没有 system 时不应凭空插入系统消息（空格占位由 convert.rs 负责）"
        );
        assert_eq!(openai["messages"][0]["role"], "user");
    }

    #[test]
    fn image_base64_source_becomes_a_data_uri() {
        let req = request_with(json!([{
            "role": "user",
            "content": [
                { "type": "text", "text": "这是什么" },
                {
                    "type": "image",
                    "source": { "type": "base64", "media_type": "image/jpeg", "data": "AAAA" }
                }
            ]
        }]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        let content = openai["messages"][0]["content"]
            .as_array()
            .expect("含图片的 user 消息必须是内容块数组");
        assert_eq!(content.len(), 2, "文本与图片都应保留");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_url");
        assert_eq!(
            content[1]["image_url"]["url"], "data:image/jpeg;base64,AAAA",
            "base64 source 必须拼成 data URI，否则上游无法解码图片"
        );
    }

    #[test]
    fn image_base64_without_media_type_uses_the_documented_fallback() {
        let req = request_with(json!([{
            "role": "user",
            "content": [
                { "type": "image", "source": { "type": "base64", "data": "BBBB" } }
            ]
        }]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(
            openai["messages"][0]["content"][0]["image_url"]["url"], "data:image/png;base64,BBBB",
            "media_type 缺失时用 image/png 兜底，不能让 data URI 变成非法形状"
        );
    }

    #[test]
    fn image_url_source_is_passed_through() {
        let req = request_with(json!([{
            "role": "user",
            "content": [
                { "type": "image", "source": { "type": "url", "url": "https://example.com/a.png" } }
            ]
        }]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(
            openai["messages"][0]["content"][0]["image_url"]["url"],
            "https://example.com/a.png"
        );
    }

    #[test]
    fn text_only_user_message_stays_a_plain_string() {
        let req = request_with(json!([{
            "role": "user",
            "content": [
                { "type": "text", "text": "前半" },
                { "type": "text", "text": "后半" }
            ]
        }]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(
            openai["messages"][0]["content"], "前半后半",
            "纯文本沿用参考实现的拼接方式（直接用字符串承载）"
        );
    }

    #[test]
    fn tool_use_and_tool_result_map_in_both_directions() {
        let req = request_with(json!([
            { "role": "user", "content": "读一下文件" },
            {
                "role": "assistant",
                "content": [
                    { "type": "thinking", "thinking": "先读文件" },
                    {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "read_file",
                        "input": { "path": "/tmp/a" }
                    }
                ]
            },
            {
                "role": "user",
                "content": [{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "文件内容" }]
            }
        ]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        let messages = openai["messages"].as_array().expect("messages 必须是数组");
        assert_eq!(messages.len(), 3, "user / assistant / tool 三条消息");

        let assistant = &messages[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(
            assistant["reasoning_content"], "先读文件",
            "PROTOCOL.md #2：thinking 必须回传成 reasoning_content，丢了上游会拒绝"
        );
        assert_eq!(assistant["tool_calls"][0]["id"], "toolu_1");
        assert_eq!(assistant["tool_calls"][0]["type"], "function");
        assert_eq!(assistant["tool_calls"][0]["function"]["name"], "read_file");
        let arguments: Value = serde_json::from_str(
            assistant["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .expect("arguments 必须是字符串"),
        )
        .expect("arguments 应是合法 JSON");
        assert_eq!(
            arguments["path"], "/tmp/a",
            "input 必须序列化成 JSON 字符串"
        );

        let tool = &messages[2];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "toolu_1");
        assert_eq!(tool["content"], "文件内容");
        assert_eq!(
            tool["name"], "read_file",
            "预扫描应把 tool_use_id 反查成工具名"
        );

        // 反方向：OpenAI 形状的 tool_calls → Anthropic 的 tool_use
        let response = build_anthropic_response(
            "msg_1",
            "claude-sonnet-4-6",
            "",
            "",
            &[json!({
                "id": "toolu_1",
                "type": "function",
                "function": { "name": "read_file", "arguments": "{\"path\":\"/tmp/a\"}" }
            })],
            "tool_calls",
            None,
        );
        assert_eq!(response["content"][0]["type"], "tool_use");
        assert_eq!(response["content"][0]["id"], "toolu_1");
        assert_eq!(response["content"][0]["name"], "read_file");
        assert_eq!(
            response["content"][0]["input"]["path"], "/tmp/a",
            "arguments 字符串必须解析回对象"
        );
    }

    #[test]
    fn tool_result_without_a_known_tool_use_omits_the_name() {
        // issue #15：历史被裁剪时找不到配对，硬塞空 name 会让上游报 Tool result is missing
        let req = request_with(json!([
            {
                "role": "user",
                "content": [{ "type": "tool_result", "tool_use_id": "toolu_gone", "content": "x" }]
            }
        ]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        let tool = &openai["messages"][0];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "toolu_gone");
        assert!(
            tool.get("name").is_none(),
            "查不到工具名时应省略 name 字段，而不是发送空字符串"
        );
    }

    #[test]
    fn tool_result_precedes_the_user_text_of_the_same_message() {
        let req = request_with(json!([
            { "role": "user", "content": "开始" },
            {
                "role": "assistant",
                "content": [
                    { "type": "tool_use", "id": "t1", "name": "f", "input": {} }
                ]
            },
            {
                "role": "user",
                "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": "结果" },
                    { "type": "text", "text": "继续" }
                ]
            }
        ]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        let messages = openai["messages"].as_array().expect("messages 必须是数组");
        assert_eq!(
            messages[2]["role"], "tool",
            "tool 消息必须紧跟 assistant 的 tool_calls"
        );
        assert_eq!(
            messages[3]["role"], "user",
            "同一条 user 消息里的文本排在 tool 结果之后"
        );
        assert_eq!(messages[3]["content"], "继续");
    }

    #[test]
    fn tool_result_array_content_is_flattened() {
        let req = request_with(json!([
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [
                        { "type": "text", "text": "第一段" },
                        { "type": "text", "text": "第二段" }
                    ]
                }]
            }
        ]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(
            openai["messages"][0]["content"], "第一段第二段",
            "tool_result 的块数组应取 text 拼接成字符串"
        );
    }

    #[test]
    fn empty_assistant_message_is_dropped() {
        let req = request_with(json!([
            { "role": "assistant", "content": "" },
            { "role": "assistant", "content": [{ "type": "text", "text": "" }] },
            { "role": "user", "content": "hi" }
        ]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(
            openai["messages"].as_array().map(Vec::len),
            Some(1),
            "没有任何内容的 assistant 消息应被丢弃，避免上游看到空轮次"
        );
    }

    #[test]
    fn unknown_content_blocks_are_ignored_not_forwarded() {
        let req = request_with(json!([
            {
                "role": "assistant",
                "content": [
                    { "type": "redacted_thinking", "data": "xxx" },
                    { "type": "text", "text": "答案" }
                ]
            },
            {
                "role": "user",
                "content": [
                    { "type": "document", "source": {} },
                    { "type": "text", "text": "问题" }
                ]
            }
        ]));
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        let messages = openai["messages"].as_array().expect("messages 必须是数组");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["content"], "答案");
        assert_eq!(messages[1]["content"], "问题");
    }

    #[test]
    fn tools_are_flattened_into_openai_functions() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["tools"] = json!([
            {
                "name": "read",
                "description": "读文件",
                "input_schema": { "type": "object", "properties": { "path": { "type": "string" } } }
            },
            { "name": "no_schema" }
        ]);
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        let tools = openai["tools"].as_array().expect("tools 必须是数组");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "read");
        assert_eq!(tools[0]["function"]["description"], "读文件");
        assert_eq!(tools[0]["function"]["parameters"]["type"], "object");
        assert_eq!(
            tools[1]["function"]["parameters"],
            json!({ "type": "object", "properties": {} }),
            "PROTOCOL.md #7：缺省 schema 必须是合法的 object 根"
        );
    }

    #[test]
    fn empty_tool_list_is_not_sent() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["tools"] = json!([]);
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert!(
            openai.get("tools").is_none(),
            "空工具数组不上送，避免上游把它当成「本轮禁用工具」"
        );
    }

    #[test]
    fn tool_choice_variants_map_to_openai_values() {
        let cases = [
            (json!({ "type": "auto" }), json!("auto")),
            (json!({ "type": "any" }), json!("required")),
            (json!({ "type": "none" }), json!("none")),
            (
                json!({ "type": "tool", "name": "read" }),
                json!({ "type": "function", "function": { "name": "read" } }),
            ),
        ];
        for (input, expected) in cases {
            let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
            req["tool_choice"] = input.clone();
            let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
            assert_eq!(
                openai["tool_choice"], expected,
                "{input} 应映射为 OpenAI 的 {expected}"
            );
        }
    }

    #[test]
    fn tool_choice_without_a_name_is_rejected() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["tool_choice"] = json!({ "type": "tool" });
        let err = anthropic_to_openai(&req).expect_err("指定工具却没给名字应报错");
        assert!(
            matches!(err, CcError::Protocol(_)),
            "缺少工具名属于非法请求，应返回 CcError::Protocol"
        );
    }

    #[test]
    fn tool_choice_without_a_type_defaults_to_auto() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["tool_choice"] = json!({});
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(
            openai["tool_choice"], "auto",
            "type 缺省时按参考实现当作 auto"
        );
    }

    #[test]
    fn stop_sequences_are_rejected_with_unsupported_option() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["stop_sequences"] = json!(["END"]);
        let err = anthropic_to_openai(&req).expect_err("stop_sequences 应被拒绝");
        assert!(
            matches!(err, CcError::UnsupportedOption(_)),
            "PROTOCOL.md #12：上游不支持 stop，必须显式报错而不是静默丢弃"
        );
    }

    #[test]
    fn empty_stop_sequences_are_accepted() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["stop_sequences"] = json!([]);
        let openai = anthropic_to_openai(&req).expect("空 stop_sequences 不应报错");
        assert!(openai.get("stop").is_none(), "也不应转发成 stop 字段");
    }

    #[test]
    fn max_tokens_defaults_when_the_client_omits_it() {
        let req = json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }] });
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(
            openai["max_tokens"], 64_000,
            "与参考实现的 max_tokens || 64000 一致；转换结果必须恒带该字段"
        );
    }

    #[test]
    fn max_tokens_of_wrong_type_is_rejected() {
        let req = json!({
            "model": "m",
            "max_tokens": "1024",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let err = anthropic_to_openai(&req).expect_err("字符串型 max_tokens 应报错");
        assert!(
            matches!(err, CcError::UnsupportedOption(_)),
            "类型错误应报 UnsupportedOption，而不是静默取默认值"
        );
    }

    #[test]
    fn temperature_top_p_and_stream_are_forwarded() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["temperature"] = json!(0.3);
        req["top_p"] = json!(0.9);
        req["stream"] = json!(true);
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(openai["temperature"], 0.3);
        assert_eq!(openai["top_p"], 0.9);
        assert_eq!(openai["stream"], true);
    }

    #[test]
    fn temperature_of_wrong_type_is_rejected() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["temperature"] = json!("hot");
        let err = anthropic_to_openai(&req).expect_err("字符串型 temperature 应报错");
        assert!(matches!(err, CcError::UnsupportedOption(_)));
    }

    #[test]
    fn metadata_user_id_becomes_the_openai_user_field() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["metadata"] = json!({ "user_id": "u-1" });
        let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
        assert_eq!(openai["user"], "u-1");
    }

    #[test]
    fn thinking_budget_maps_to_reasoning_effort() {
        let cases = [
            (
                json!({ "type": "enabled", "budget_tokens": 12_000 }),
                Some("high"),
            ),
            (
                json!({ "type": "enabled", "budget_tokens": 5_000 }),
                Some("medium"),
            ),
            (
                json!({ "type": "enabled", "budget_tokens": 2_000 }),
                Some("low"),
            ),
            (
                json!({ "type": "enabled", "budget_tokens": 100 }),
                Some("low"),
            ),
            (
                json!({ "type": "adaptive", "effort": "xhigh" }),
                Some("xhigh"),
            ),
            (json!({ "type": "adaptive" }), Some("medium")),
            (json!({ "type": "disabled" }), None),
            (json!({ "type": "none" }), None),
        ];
        for (thinking, expected) in cases {
            let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
            req["thinking"] = thinking.clone();
            let openai = anthropic_to_openai(&req).expect("合法请求应转换成功");
            match expected {
                Some(effort) => assert_eq!(
                    openai["reasoning_effort"], effort,
                    "{thinking} 应映射为 reasoning_effort={effort}"
                ),
                None => assert!(
                    openai.get("reasoning_effort").is_none(),
                    "{thinking} 不应产生 reasoning_effort"
                ),
            }
        }
    }

    #[test]
    fn non_object_request_and_non_array_messages_are_rejected() {
        let err = anthropic_to_openai(&json!("nope")).expect_err("非对象请求应报错");
        assert!(matches!(err, CcError::Protocol(_)));

        let err = anthropic_to_openai(&json!({ "model": "m", "messages": "nope" }))
            .expect_err("messages 不是数组应报错");
        assert!(matches!(err, CcError::Protocol(_)));
    }

    #[test]
    fn input_tokens_subtract_the_cached_portion() {
        // PROTOCOL.md #7 / issue #25：Anthropic 的 input_tokens 只计非缓存部分
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 5,
            cached_input_tokens: 30,
        };
        assert_eq!(
            input_tokens_for_anthropic(usage),
            70,
            "prompt 总数含缓存，必须减掉 cache_read 才是 Anthropic 的 input_tokens"
        );
    }

    #[test]
    fn input_tokens_clamp_at_zero_when_cache_exceeds_the_total() {
        let equal = Usage {
            input_tokens: 50,
            output_tokens: 1,
            cached_input_tokens: 50,
        };
        assert_eq!(
            input_tokens_for_anthropic(equal),
            0,
            "恰好全部命中缓存时应为 0"
        );

        let over = Usage {
            input_tokens: 10,
            output_tokens: 1,
            cached_input_tokens: 999,
        };
        assert_eq!(
            input_tokens_for_anthropic(over),
            0,
            "缓存大于总数时夹到 0，绝不能回绕成天文数字"
        );
    }

    #[test]
    fn input_tokens_without_cache_are_unchanged() {
        let usage = Usage {
            input_tokens: 42,
            output_tokens: 1,
            cached_input_tokens: 0,
        };
        assert_eq!(input_tokens_for_anthropic(usage), 42);
    }

    #[test]
    fn stop_reason_mapping_covers_both_naming_schemes() {
        assert_eq!(map_stop_reason("tool_calls"), "tool_use");
        assert_eq!(map_stop_reason("tool-calls"), "tool_use");
        assert_eq!(map_stop_reason("tool_use"), "tool_use");
        assert_eq!(map_stop_reason("length"), "max_tokens");
        assert_eq!(map_stop_reason("max-tokens"), "max_tokens");
        assert_eq!(map_stop_reason("stop_sequence"), "stop_sequence");
        assert_eq!(map_stop_reason("stop"), "end_turn");
        assert_eq!(
            map_stop_reason("brand-new-reason"),
            "end_turn",
            "未知取值必须退化为合法枚举，透传会让 Anthropic SDK 解析失败"
        );
    }

    #[test]
    fn sse_event_order_matches_the_anthropic_state_machine() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "claude-sonnet-4-6");
        let mut events = Vec::new();
        events.extend(builder.push(&UpstreamEvent::ReasoningStart));
        events.extend(builder.push(&UpstreamEvent::ReasoningDelta("想一想".into())));
        events.extend(builder.push(&UpstreamEvent::TextDelta("答案".into())));
        events.extend(builder.push(&UpstreamEvent::ToolCall {
            id: "toolu_1".into(),
            name: "read".into(),
            input: json!({ "path": "/tmp/a" }),
        }));
        events.extend(builder.push(&UpstreamEvent::Finish {
            finish_reason: Some("tool-calls".into()),
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 7,
                cached_input_tokens: 40,
            }),
        }));
        events.extend(builder.finish());

        let names = event_names(&events);
        assert_eq!(
            names.first().copied(),
            Some("message_start"),
            "message_start 必须是第一个事件，否则 SDK 会拒绝整条流"
        );
        assert_eq!(
            names.last().copied(),
            Some("message_stop"),
            "message_stop 必须是最后一个事件"
        );
        let last_stop = names
            .iter()
            .rposition(|name| *name == "content_block_stop")
            .expect("应至少有一个 content_block_stop");
        let message_delta = names
            .iter()
            .position(|name| *name == "message_delta")
            .expect("应有 message_delta");
        assert!(
            last_stop < message_delta,
            "所有 content_block_stop 必须早于 message_delta，实际顺序：{names:?}"
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| **name == "message_start")
                .count(),
            1,
            "message_start 只能出现一次"
        );
        assert_eq!(
            names.iter().filter(|name| **name == "message_stop").count(),
            1,
            "message_stop 只能出现一次"
        );

        // 块下标必须递增，且 thinking 块先于 text 块先于 tool_use 块
        let starts: Vec<(u64, &str)> = events
            .iter()
            .filter(|event| event.event == "content_block_start")
            .map(|event| {
                (
                    event.data["index"].as_u64().expect("index 必须是数字"),
                    event.data["content_block"]["type"]
                        .as_str()
                        .expect("content_block.type 必须是字符串"),
                )
            })
            .collect();
        assert_eq!(
            starts,
            vec![(0, "thinking"), (1, "text"), (2, "tool_use")],
            "块下标应从 0 递增，且类型切换时先关后开"
        );
    }

    #[test]
    fn message_delta_carries_stop_reason_and_subtracted_usage() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        builder.push(&UpstreamEvent::TextDelta("hi".into()));
        builder.push(&UpstreamEvent::Finish {
            finish_reason: Some("tool_calls".into()),
            usage: Some(Usage {
                input_tokens: 100,
                output_tokens: 5,
                cached_input_tokens: 30,
            }),
        });
        let events = builder.finish();
        let delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("finish 应产出 message_delta");
        assert_eq!(delta.data["delta"]["stop_reason"], "tool_use");
        assert_eq!(delta.data["usage"]["output_tokens"], 5);
        assert_eq!(
            delta.data["usage"]["input_tokens"], 70,
            "message_delta 的 input_tokens 必须做减法（issue #25）"
        );
        assert_eq!(delta.data["usage"]["cache_read_input_tokens"], 30);
        assert_eq!(delta.data["usage"]["cache_creation_input_tokens"], 0);
    }

    #[test]
    fn message_delta_defaults_to_end_turn_without_a_finish_event() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        builder.push(&UpstreamEvent::TextDelta("hi".into()));
        let events = builder.finish();
        let delta = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("即使上游断流也要收尾");
        assert_eq!(
            delta.data["delta"]["stop_reason"], "end_turn",
            "没有 finish 事件时给出合法默认值，避免客户端一直等待"
        );
    }

    #[test]
    fn thinking_block_emits_a_signature_delta_that_varies_with_the_text() {
        let mut first = AnthropicSseBuilder::new("msg_1", "m");
        first.push(&UpstreamEvent::ReasoningDelta("第一段思考".into()));
        let first_events = first.finish();
        let first_signatures = signatures(&first_events);
        assert_eq!(first_signatures.len(), 1, "thinking 块关闭前必须有签名");
        assert!(
            first_signatures[0].starts_with('E'),
            "伪造签名的首字符必须是 'E'（protobuf field 2 的单层信封），否则 Claude Code 不认"
        );

        let mut second = AnthropicSseBuilder::new("msg_2", "m");
        second.push(&UpstreamEvent::ReasoningDelta("另一段思考".into()));
        let second_signatures = signatures(&second.finish());
        assert_ne!(
            first_signatures[0], second_signatures[0],
            "签名必须随 thinking 文本变化，重复签名会触发客户端的去重异常"
        );

        // 签名事件必须排在对应块的 content_block_stop 之前
        let names = event_names(&first_events);
        let signature_at = names
            .iter()
            .position(|name| *name == "content_block_delta")
            .expect("应有 delta");
        let stop_at = names
            .iter()
            .position(|name| *name == "content_block_stop")
            .expect("应有 stop");
        assert!(
            signature_at < stop_at,
            "signature_delta 必须在 content_block_stop 之前发出"
        );
    }

    #[test]
    fn text_blocks_do_not_carry_a_signature() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        builder.push(&UpstreamEvent::TextDelta("正文".into()));
        let events = builder.finish();
        assert!(
            signatures(&events).is_empty(),
            "只有 thinking 块需要签名，正文块加签名会被 SDK 判为非法"
        );
    }

    #[test]
    fn tool_use_block_streams_its_input_as_partial_json() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        let events = builder.push(&UpstreamEvent::ToolCall {
            id: "toolu_9".into(),
            name: "read_file".into(),
            input: json!({ "path": "/tmp/a" }),
        });
        let start = events
            .iter()
            .find(|event| event.event == "content_block_start")
            .expect("工具块应有一个 start");
        assert_eq!(start.data["content_block"]["type"], "tool_use");
        assert_eq!(start.data["content_block"]["id"], "toolu_9");
        assert_eq!(start.data["content_block"]["name"], "read_file");
        assert_eq!(
            start.data["content_block"]["input"],
            json!({}),
            "Anthropic 的 tool_use 起始块 input 必须为空对象，实参走 input_json_delta"
        );
        let delta = events
            .iter()
            .find(|event| event.data["delta"]["type"] == "input_json_delta")
            .expect("工具块应有 input_json_delta");
        let partial: Value = serde_json::from_str(
            delta.data["delta"]["partial_json"]
                .as_str()
                .expect("partial_json 必须是字符串"),
        )
        .expect("partial_json 应是合法 JSON");
        assert_eq!(partial["path"], "/tmp/a");
        assert_eq!(
            events.last().map(|event| event.event),
            Some("content_block_stop"),
            "工具块在一次 push 内开闭"
        );
    }

    #[test]
    fn tool_call_without_an_id_gets_a_deterministic_one() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        let events = builder.push(&UpstreamEvent::ToolCall {
            id: String::new(),
            name: "f".into(),
            input: json!({}),
        });
        let start = events
            .iter()
            .find(|event| event.event == "content_block_start")
            .expect("应有工具块 start");
        assert_eq!(
            start.data["content_block"]["id"], "toolu_0",
            "缺失的 id 必须补一个（确定性生成，便于测试与排障）"
        );
    }

    #[test]
    fn finish_is_idempotent() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        builder.push(&UpstreamEvent::TextDelta("hi".into()));
        let first = builder.finish();
        assert!(!first.is_empty());
        assert!(
            builder.finish().is_empty(),
            "重复收尾不得产生第二个 message_stop（SDK 会把它当成新的一轮）"
        );
        assert!(
            builder
                .push(&UpstreamEvent::TextDelta("晚了".into()))
                .is_empty(),
            "收尾后到达的事件必须被丢弃"
        );
    }

    #[test]
    fn finish_without_any_upstream_event_still_opens_and_closes_the_stream() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        let names = event_names(&builder.finish());
        assert_eq!(
            names,
            vec!["message_start", "message_delta", "message_stop"],
            "没有任何上游事件时也必须给出完整且合法的骨架"
        );
        assert_eq!(
            builder.output_tokens(),
            0,
            "零输出由调用方判定（参考实现会改判成 429），builder 本身不吞掉收尾事件"
        );
    }

    #[test]
    fn zero_output_tokens_zeroes_the_whole_usage() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        builder.push(&UpstreamEvent::Finish {
            finish_reason: Some("stop".into()),
            usage: Some(Usage {
                input_tokens: 999,
                output_tokens: 0,
                cached_input_tokens: 100,
            }),
        });
        let delta = builder
            .finish()
            .into_iter()
            .find(|event| event.event == "message_delta")
            .expect("应有 message_delta");
        assert_eq!(
            delta.data["usage"]["input_tokens"], 0,
            "防伪账：output=0 时整体清零（与 openai.rs 的 normalized 一致）"
        );
        assert_eq!(delta.data["usage"]["output_tokens"], 0);
    }

    #[test]
    fn empty_deltas_do_not_open_blocks() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        let events = builder.push(&UpstreamEvent::TextDelta(String::new()));
        assert_eq!(
            event_names(&events),
            vec!["message_start"],
            "空增量不应打开内容块，否则会留下一对空 start/stop"
        );
        let events = builder.push(&UpstreamEvent::ReasoningDelta(String::new()));
        assert!(
            !event_names(&events).contains(&"content_block_start"),
            "空 thinking 增量同样不应打开块（空块没有签名可派生）"
        );
    }

    #[test]
    fn error_event_produces_no_termination_events() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        let events = builder.push(&UpstreamEvent::Error {
            message: "boom".into(),
            code: None,
        });
        assert_eq!(
            event_names(&events),
            vec!["message_start"],
            "error 事件不得自行收尾，否则会吞掉后续真正的 tool_calls 结束原因"
        );
    }

    #[test]
    fn structural_events_produce_no_block_changes() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        for event in [
            UpstreamEvent::TextStart,
            UpstreamEvent::TextEnd,
            UpstreamEvent::ReasoningStart,
            UpstreamEvent::ReasoningEnd,
            UpstreamEvent::Ignored,
        ] {
            let names = event_names(&builder.push(&event));
            assert!(
                !names.iter().any(|name| name.starts_with("content_block")),
                "{event:?} 不应产生内容事件"
            );
        }
    }

    #[test]
    fn switching_between_thinking_and_text_closes_the_previous_block() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        let mut events = builder.push(&UpstreamEvent::ReasoningDelta("想".into()));
        events.extend(builder.push(&UpstreamEvent::TextDelta("说".into())));
        let names = event_names(&events);
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
            ],
            "类型切换时必须先补 signature_delta 再 stop，然后开新块"
        );
    }

    #[test]
    fn non_streaming_response_shape_matches_anthropic() {
        let response = build_anthropic_response(
            "msg_1",
            "claude-sonnet-4-6",
            "答案",
            "思考",
            &[json!({
                "id": "toolu_1",
                "type": "function",
                "function": { "name": "read", "arguments": "{\"path\":\"/tmp/a\"}" }
            })],
            "tool_calls",
            Some(Usage {
                input_tokens: 100,
                output_tokens: 5,
                cached_input_tokens: 30,
            }),
        );
        assert_eq!(response["type"], "message");
        assert_eq!(response["role"], "assistant");
        assert_eq!(response["id"], "msg_1");
        assert_eq!(response["model"], "claude-sonnet-4-6");
        assert_eq!(response["stop_reason"], "tool_use");
        assert_eq!(
            response["stop_sequence"],
            Value::Null,
            "不支持 stop 序列（PROTOCOL.md #12），该字段恒为 null"
        );
        let content = response["content"].as_array().expect("content 必须是数组");
        assert_eq!(content.len(), 3, "thinking + text + tool_use 三个块");
        assert_eq!(content[0]["type"], "thinking");
        assert!(
            content[0]["signature"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "thinking 块必须带非空签名"
        );
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "答案");
        assert_eq!(content[2]["type"], "tool_use");
        assert_eq!(response["usage"]["input_tokens"], 70);
        assert_eq!(response["usage"]["output_tokens"], 5);
        assert_eq!(response["usage"]["cache_read_input_tokens"], 30);
    }

    #[test]
    fn non_streaming_response_estimates_output_tokens_when_usage_is_missing() {
        let response = build_anthropic_response(
            "msg_1",
            "m",
            "一段足够长的正文",
            "以及一段思考",
            &[],
            "stop",
            None,
        );
        let output = response["usage"]["output_tokens"]
            .as_u64()
            .expect("output_tokens 必须是数字");
        assert!(
            output > 0,
            "上游没回 usage 时按内容长度估算，绝不能是 0（面板会显示成零输出）"
        );
        assert_eq!(response["stop_reason"], "end_turn");
    }

    #[test]
    fn converted_request_survives_the_second_stage_into_the_generate_body() {
        // 两段式转换的接缝：anthropic_to_openai 的产物必须能被 convert.rs 直接消费，
        // 并且保留 PROTOCOL.md #1（system 是字符串）与 #2（reasoning 最前）两条硬约束。
        let req = json!({
            "model": "deepseek/deepseek-v4-flash",
            "max_tokens": 2048,
            "system": [
                { "type": "text", "text": "系统甲" },
                { "type": "text", "text": "系统乙" }
            ],
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "thinking", "thinking": "先想" },
                        { "type": "text", "text": "再答" },
                        { "type": "tool_use", "id": "t1", "name": "f", "input": {} }
                    ]
                },
                {
                    "role": "user",
                    "content": [{ "type": "tool_result", "tool_use_id": "t1", "content": "结果" }]
                }
            ]
        });
        let openai = anthropic_to_openai(&req).expect("第一段转换应成功");
        let body = crate::convert::build_generate_body(&openai, &crate::config::Config::default())
            .expect("第二段转换应成功");
        assert!(
            body["params"]["system"].is_string(),
            "PROTOCOL.md #1：跨两段转换后 params.system 仍必须是字符串"
        );
        assert_eq!(body["params"]["system"], "系统甲\n系统乙");
        // messages[0] 是 system（上一步的断言），assistant 是 messages[1]
        let blocks = body["params"]["messages"][1]["content"]
            .as_array()
            .expect("assistant 的 content 必须是数组");
        assert_eq!(
            blocks[0]["type"], "reasoning",
            "PROTOCOL.md #2：reasoning 必须保持在内容块数组最前"
        );
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[2]["type"], "tool-call");
        assert_eq!(blocks[2]["toolCallId"], "t1");
        assert_eq!(
            body["params"]["max_tokens"], 2048,
            "max_tokens 应原样带过去"
        );
    }

    #[test]
    fn sse_event_serialization_uses_the_wire_format() {
        let mut builder = AnthropicSseBuilder::new("msg_1", "m");
        let events = builder.push(&UpstreamEvent::TextDelta("hi".into()));
        let frame = events[0].to_sse();
        assert!(
            frame.starts_with("event: message_start\ndata: "),
            "帧必须以 event 名开头，实际：{frame}"
        );
        assert!(frame.ends_with("\n\n"), "SSE 帧必须以空行结束");
        let payload = frame
            .trim_start_matches("event: message_start\ndata: ")
            .trim_end();
        let parsed: Value = serde_json::from_str(payload).expect("data 必须是合法 JSON");
        assert_eq!(parsed["type"], "message_start");
    }
}
