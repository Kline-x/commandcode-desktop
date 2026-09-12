//! OpenAI Chat Completions 请求体 → Command Code /alpha/generate 请求体的转换。
//!
//! 本模块是**纯函数**层：不读系统时间、不碰磁盘、不发网络请求。日期与环境描述由调用方
//! 经参数注入（STYLE.md 2.3），因此全部行为都能被确定性单测覆盖。
//!
//! 协议依据：docs/PROTOCOL.md 第 2 节（请求形状）与第 4 节（硬约束坑位表）。
//! 上游参考实现是 third_party/proxy.mjs 的 buildCcRequest（约 439-610 行）与
//! tryParseJSON（约 612-614 行）；参考实现里带 issue 编号或「真机验证」标注的坑，
//! 在下方逐条保留了说明，**不要**在没有重新抓包的情况下「优化」掉。

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::error::CcError;

/// params.max_tokens 的硬上限。
///
/// 参考实现用 Math.min(max_tokens || 64000, 200000)：超过 200000 上游直接拒绝，
/// 因此这里做**夹紧**而不是报错。
pub const MAX_GENERATE_TOKENS: u64 = 200_000;

/// 模型名缺省或为空时的兜底值（与参考实现一致）。
const DEFAULT_MODEL: &str = "deepseek/deepseek-v4-flash";

/// 查不到配对的 assistant tool_calls 时，给 tool 消息的 toolName。
///
/// 参考实现在这种情况下回退到空串。空串会让模型看到「没有名字的工具结果」，
/// 既难排障也容易让模型误判；上游并不校验这个名字是否真的在 tool-call 里出现过，
/// 所以用一个显式可读的占位值更安全。
const UNKNOWN_TOOL_NAME: &str = "unknown";

/// 无 system prompt 时的占位内容。
///
/// PROTOCOL.md #3：上游在 params.system 缺省时会注入自身约 7.5K token 的默认提示词
/// （真机验证：prompt_tokens 从 85 涨到 7653，见 issue #17），且会让模型以为自己在
/// CC 的可执行目录里。发**一个空格**即可绕过。
const EMPTY_SYSTEM_PLACEHOLDER: &str = " ";

/// 构建 /alpha/generate 请求体所需的环境快照。
///
/// 这些字段描述「这台机器上的这个项目长什么样」，属于 I/O 边界，必须由调用方采集后
/// 注入——否则本模块就无法在测试里构造确定性输入（STYLE.md 2.3）。
/// 字段名与上游 config 对象的 camelCase 键一一对应（PROTOCOL.md 第 2 节）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GenerateContext {
    /// 工作目录（上游 config.workingDir）。
    pub working_dir: String,
    /// 日期，形如 YYYY-MM-DD（由调用方按目标时区算好，**不在这里读系统时间**）。
    pub date: String,
    /// 运行环境描述，例如 darwin-arm64, Node.js 22.0.0。
    pub environment: String,
    /// 目录结构摘要（上游 config.structure）。
    pub structure: Vec<String>,
    /// 是否处于 git 仓库。
    pub is_git_repo: bool,
    /// 当前分支。
    pub current_branch: String,
    /// 主分支（main / master）。
    pub main_branch: String,
    /// git status --short 的输出。
    pub git_status: String,
    /// 最近提交摘要。
    pub recent_commits: Vec<String>,
}

/// 从 OpenAI 的 messages 里提取系统提示，展开数组型 content 并拼接成**单个字符串**。
///
/// PROTOCOL.md #1：params.system 恒为字符串。数组型 content 必须**展开取 text 后拼接**，
/// 而不是 JSON.stringify 成字符串，更不能输出 Anthropic 风格的块数组，否则上游直接
/// 拒绝：Validation error: Invalid input: expected string, received array at "params.system"。
///
/// system 与 developer 都算系统提示（后者在 OpenAI 语义里优先级更高，但上游只有一个
/// system 字段，只能按出现顺序拼接，与参考实现一致）。
///
/// 返回 None 表示**一条系统消息都没有**。调用方对本函数的返回值做的是「falsy 判断」：
/// 空字符串与 None 走同一条占位分支，与参考实现的
/// if (systemPrompt) ... else if (CFG.emptySystemPlaceholder) 一致。
pub fn extract_system_prompt(messages: &[Value]) -> Option<String> {
    let mut parts = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if role != "system" && role != "developer" {
            continue;
        }
        parts.push(system_message_text(message));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// 把任意 JSON 值转成文本；对象 / 数组退化为 JSON 序列化。
///
/// 参考实现是 JS 的 String(msg.content)，对任何类型都不会抛错；Rust 里没有直接等价物，
/// 这里保持同样的宽松语义：**永不失败**，宁可交出可读的 JSON 也不要 panic
/// （STYLE.md 2.4：库代码不得因运行时输入 panic）。
fn scalar_to_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// 把单条 system / developer 消息的 content 展平成文本。
fn system_message_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        // 数组块：取 text（Anthropic 风格块则取 content）；拼不回结构化数据是**故意的**
        // ——上游这一层只要纯文本（PROTOCOL.md #1）。
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| {
                block
                    .get("text")
                    .or_else(|| block.get("content"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::Null) | None => String::new(),
        Some(other) => scalar_to_text(other),
    }
}

/// 把 image_url 类型的内容块转成 CC 的 {type:"image", image:"data:..."}。
///
/// CC CLI 的抓包格式就是 data: 开头的 base64 data URL，但上游同样接受 https:// 直链，
/// 因此这里**原样透传 URL、不做协议校验**。OpenAI 侧有两种写法都要认：
/// {"type":"image_url","image_url":{"url":...}}，以及被部分 SDK / 中转拍平的
/// {"type":"image_url","url":...}。
fn image_part_from_openai(part: &Value) -> Value {
    let url = part
        .get("image_url")
        .and_then(|image_url| image_url.get("url"))
        .and_then(Value::as_str)
        .or_else(|| part.get("image_url").and_then(Value::as_str))
        .or_else(|| part.get("url").and_then(Value::as_str))
        .unwrap_or_default();
    json!({ "type": "image", "image": url })
}

/// 转换单个 user 内容块；无法识别的块返回 None 由调用方丢弃。
///
/// input_text / input_image 是 Responses API 的块名，一并接受——同一批客户端会在两套
/// API 之间切换。input_audio / file 这类 CC 端没有对应块的模态一律**丢弃而不是透传**：
/// 透传会让上游校验整个请求失败，丢掉最多是少一个模态。
fn convert_user_part(part: &Value) -> Option<Value> {
    match part.get("type").and_then(Value::as_str) {
        Some("image_url") | Some("input_image") => Some(image_part_from_openai(part)),
        Some("text") | Some("input_text") => {
            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
            if text.is_empty() {
                return None;
            }
            Some(json!({ "type": "text", "text": text }))
        }
        _ => None,
    }
}

/// 转换 user 消息的 content 为 CC 的内容块数组。
///
/// 返回 None 表示转换后**没有任何内容块**，调用方应整条丢弃：空 content 的 user 消息
/// 会让上游把前后两个 assistant 轮次看成一次连续输出，也浪费一次校验。
fn convert_user_content(content: Option<&Value>) -> Option<Vec<Value>> {
    match content {
        Some(Value::String(text)) => {
            if text.is_empty() {
                return None;
            }
            Some(vec![json!({ "type": "text", "text": text })])
        }
        Some(Value::Array(parts)) => {
            let converted = parts
                .iter()
                .filter_map(convert_user_part)
                .collect::<Vec<_>>();
            if converted.is_empty() {
                None
            } else {
                Some(converted)
            }
        }
        Some(Value::Null) | None => None,
        // 参考实现走「把标量当文本」的分支，这里保持一致
        Some(other) => {
            let text = scalar_to_text(other);
            if text.is_empty() {
                None
            } else {
                Some(vec![json!({ "type": "text", "text": text })])
            }
        }
    }
}

/// 解析 tool_call 的 function.arguments。
///
/// OpenAI 协议里它是**字符串**（流式拼接的自然结果），但也有人直接塞已解析的对象，
/// 两种都要接受（对应参考实现的 tryParseJSON）。
/// 解析失败时退化为空对象而**不是**报错：工具参数坏了应该让模型看到空参数后重试，
/// 而不是让整个请求 400——上游对畸形参数的容忍度远高于对畸形请求体的容忍度。
fn parse_tool_arguments(arguments: &Value) -> Value {
    match arguments {
        Value::String(raw) => {
            let trimmed = raw.trim();
            // 空字符串在 OpenAI 语义里等价于「无参数」
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

/// 把 OpenAI 的 tool_calls 转成 CC 的 tool-call 块。
///
/// PROTOCOL.md #5：任何 tool-call 都必须有配对的 tool result 才回放。
/// 没有 id 的调用**无法**被后续 tool 消息配对（回放它只会让上游报
/// Tool result is missing），因此这里直接丢弃；配对本身由调用方保证。
fn convert_tool_calls(tool_calls: Option<&Value>) -> Vec<Value> {
    let Some(Value::Array(calls)) = tool_calls else {
        return Vec::new();
    };
    calls
        .iter()
        .filter_map(|call| {
            let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
            if id.is_empty() {
                return None;
            }
            let function = call.get("function");
            let name = function
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let input = function
                .and_then(|function| function.get("arguments"))
                .map(parse_tool_arguments)
                .unwrap_or_else(|| json!({}));
            Some(json!({
                "type": "tool-call",
                "toolCallId": id,
                "toolName": name,
                "input": input,
            }))
        })
        .collect()
}

/// 转换 assistant 消息的内容块数组。
///
/// 次序被上游硬性校验：**reasoning 必须在最前**，然后是 text，最后是 tool-call
/// （PROTOCOL.md #2：thinking 模式下丢弃 reasoning 会被直接拒绝）。
/// 参考实现还保留了「客户端把 reasoning 直接写进 content 数组」的写法，并在已有
/// reasoning_content 字段时**不重复**加入——这里同样实现，因为 OpenAI SDK 与把
/// Anthropic 块塞进来的转发层两种写法都会出现。
fn convert_assistant_content(message: &Value) -> Vec<Value> {
    let mut parts = Vec::new();
    let reasoning_content = message
        .get("reasoning_content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !reasoning_content.is_empty() {
        parts.push(json!({ "type": "reasoning", "text": reasoning_content }));
    }
    match message.get("content") {
        Some(Value::String(text)) => {
            if !text.is_empty() {
                parts.push(json!({ "type": "text", "text": text }));
            }
        }
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !text.is_empty() {
                            parts.push(json!({ "type": "text", "text": text }));
                        }
                    }
                    // 字段形式优先；数组形式只在没有该字段时透传，避免 reasoning 出现两次
                    Some("reasoning") if reasoning_content.is_empty() => {
                        let text = block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !text.is_empty() {
                            parts.push(json!({ "type": "reasoning", "text": text }));
                        }
                    }
                    Some("image_url") | Some("input_image") => {
                        parts.push(image_part_from_openai(block));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    parts.extend(convert_tool_calls(message.get("tool_calls")));
    parts
}

/// 把 tool 消息的 content 展平成字符串。
///
/// OpenAI 允许 tool content 是内容块数组（例如把结果包成 text 块），但 CC 的
/// output.value 是字符串字段，因此数组走 JSON 序列化——与参考实现的
/// JSON.stringify(msg.content) 一致。
fn tool_output_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 建立 tool_call_id → tool_name 反查表。
///
/// OpenAI 的 tool 消息只带 tool_call_id，不带工具名，所以必须回头查 assistant 的
/// tool_calls。表覆盖**全部** chat 消息（与参考实现一致），因此理论上「后面的
/// assistant」也能补上前面的 tool 结果；同 id 后出现者覆盖先出现者。
fn build_tool_name_index(messages: &[Value]) -> BTreeMap<String, String> {
    let mut index = BTreeMap::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if role != "assistant" {
            continue;
        }
        let Some(Value::Array(calls)) = message.get("tool_calls") else {
            continue;
        };
        for call in calls {
            let Some(id) = call.get("id").and_then(Value::as_str) else {
                continue;
            };
            let name = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            index.insert(id.to_string(), name.to_string());
        }
    }
    index
}

/// 转换 tool 消息为 CC 的 tool-result 块。
///
/// toolName 优先从 assistant 的 tool_calls 查表，其次回退到消息自带的 name
/// （参考实现如此），最后落到 UNKNOWN_TOOL_NAME——**不回退到空串**，理由见该常量的注释。
/// tool 消息本身**不丢弃**：即使查不到名字，上游也需要一条 tool-result 来配对
/// （PROTOCOL.md #5）。
fn convert_tool_message(message: &Value, tool_names: &BTreeMap<String, String>) -> Value {
    let tool_call_id = message
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let tool_name = tool_names
        .get(tool_call_id)
        .map(String::as_str)
        .or_else(|| message.get("name").and_then(Value::as_str))
        .filter(|name| !name.is_empty())
        .unwrap_or(UNKNOWN_TOOL_NAME);
    let output = message
        .get("content")
        .map(tool_output_text)
        .unwrap_or_default();
    json!({
        "role": "tool",
        "content": [{
            "type": "tool-result",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "output": { "type": "text", "value": output },
        }],
    })
}

/// 转换单条非 system 消息；返回 None 表示该消息应被整条丢弃。
///
/// PROTOCOL.md #6：未知 role 归一化为 user 且 content 恒为数组——上游会拒绝任何别的形状。
fn convert_message(message: &Value, tool_names: &BTreeMap<String, String>) -> Option<Value> {
    match message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "assistant" => {
            let content = convert_assistant_content(message);
            // 空 assistant（既无文本也无工具调用）没有信息量，留下只会让上游看到空数组
            if content.is_empty() {
                None
            } else {
                Some(json!({ "role": "assistant", "content": content }))
            }
        }
        "tool" => Some(convert_tool_message(message, tool_names)),
        // user 以及所有未知 role
        _ => {
            let content = convert_user_content(message.get("content"))?;
            Some(json!({ "role": "user", "content": content }))
        }
    }
}

/// 转换 OpenAI 的 tools 为 CC 的扁平形状。
///
/// OpenAI：{type, function:{name, description, parameters}}；
/// CC：{type, name, description, input_schema}。
/// PROTOCOL.md #7：input_schema 的根必须是 type:"object"，第三方手写 schema / MCP 会导致
/// 整轮失败。这里只保证「缺省时给一个合法的空 object schema」，**不**改写调用方显式
/// 给出的 schema——擅自包装会改变模型看到的参数结构。
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
        // 参考实现是 if (tools && tools.length > 0)：空数组不上送
        return Ok(None);
    }
    let converted = items.iter().map(convert_tool).collect::<Vec<_>>();
    Ok(Some(converted))
}

/// 转换单个工具定义（供 convert_tools 使用）。
fn convert_tool(tool: &Value) -> Value {
    let function = tool.get("function");
    let name = function
        .and_then(|function| function.get("name"))
        .or_else(|| tool.get("name"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let description = function
        .and_then(|function| function.get("description"))
        .or_else(|| tool.get("description"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input_schema = function
        .and_then(|function| function.get("parameters"))
        .or_else(|| tool.get("input_schema"))
        .cloned()
        .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
    json!({
        "type": tool.get("type").and_then(Value::as_str).unwrap_or("function"),
        "name": name,
        "description": description,
        "input_schema": input_schema,
    })
}

/// 转换 OpenAI 的 tool_choice 为 CC（Anthropic 风格）的取值。
///
/// - 字符串：auto → {type:"auto"}、none → {type:"none"}、required → {type:"any"}；
///   未知字符串按参考实现退化为 auto（上游对未知取值的拒绝信息很难排障，
///   而 auto 是语义上最接近「没指定」的选项）。
/// - 对象且 type == "function"：{type:"tool", name}；缺 name 直接报错，
///   因为「指定了一个没有名字的工具」在两边都是非法请求。
/// - 其他对象：原样透传（客户端可能已经给了 CC / Anthropic 形状，例如 {type:"auto"}）。
fn convert_tool_choice(tool_choice: Option<&Value>) -> Result<Option<Value>, CcError> {
    let Some(tool_choice) = tool_choice else {
        return Ok(None);
    };
    match tool_choice {
        Value::Null => Ok(None),
        Value::String(name) => {
            let mapped = match name.as_str() {
                "none" => "none",
                "required" => "any",
                _ => "auto",
            };
            Ok(Some(json!({ "type": mapped })))
        }
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("function") {
                let name = object
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| {
                        CcError::Protocol(
                            "tool_choice.type=function 时必须给出 function.name".to_string(),
                        )
                    })?;
                return Ok(Some(json!({ "type": "tool", "name": name })));
            }
            Ok(Some(tool_choice.clone()))
        }
        _ => Err(CcError::UnsupportedOption(
            "tool_choice 只支持字符串或对象".to_string(),
        )),
    }
}

/// 读取可选的字符串参数（model / reasoning_effort 之类）。
///
/// 类型不对时报 CcError::UnsupportedOption 而不是静默忽略：静默忽略会让调用方以为
/// 参数生效了，排查成本极高。null 与缺省同义。
fn optional_string(req: &Map<String, Value>, key: &str) -> Result<Option<String>, CcError> {
    match req.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(CcError::UnsupportedOption(format!("{key} 必须是字符串"))),
    }
}

/// 读取 max_tokens 并按上游上限夹紧。
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
    // 参考实现是 max_tokens || 64000：0 在 JS 里是假值，与「未提供」同义
    let effective = if requested == 0 {
        crate::config::DEFAULT_GENERATE_MAX_TOKENS
    } else {
        requested
    };
    Ok(effective.min(MAX_GENERATE_TOKENS))
}

/// 把 OpenAI Chat Completions 请求体转换成 /alpha/generate 请求体。
///
/// 这是**无环境快照**的便捷入口，等价于传入 GenerateContext::default()（各字段为空）。
/// 真实代理路径应当调用 build_generate_body_with_context 并注入采集好的工作目录、
/// 日期与 git 信息，否则上游会看到空的环境描述。
pub fn build_generate_body(req: &Value, cfg: &Config) -> Result<Value, CcError> {
    build_generate_body_with_context(req, cfg, &GenerateContext::default())
}

/// 把 OpenAI Chat Completions 请求体转换成 /alpha/generate 请求体（带环境快照）。
///
/// 转换后的形状见 PROTOCOL.md 第 2 节：
/// { config, memory: null, taste: null, skills: "", permissionMode, params }。
///
/// 关键约束（全部来自真机验证，见 PROTOCOL.md 第 4 节）：
/// - params.system 恒为字符串（#1）；无 system 时按 cfg.empty_system_placeholder
///   发一个空格（#3）；
/// - assistant 的 reasoning 必须在内容块数组最前（#2）；
/// - 未知 role 归一化为 user（#6）；
/// - max_tokens 夹紧到 MAX_GENERATE_TOKENS；
/// - stream 恒为 true，因为 /alpha/generate 只有流式；非流式请求由本地代理缓冲后
///   一次性返回，不能把这个字段透传给上游。
///
/// 非法输入一律返回 CcError::Protocol / CcError::UnsupportedOption，不 panic。
pub fn build_generate_body_with_context(
    req: &Value,
    cfg: &Config,
    context: &GenerateContext,
) -> Result<Value, CcError> {
    let object = req
        .as_object()
        .ok_or_else(|| CcError::Protocol("OpenAI 请求体必须是 JSON 对象".to_string()))?;

    // PROTOCOL.md #12：上游不支持 stop 序列，带 stop 的请求直接失败。静默丢弃会让
    // 调用方以为停止序列生效了，因此显式报错。
    if let Some(stop) = object.get("stop") {
        if !stop.is_null() {
            return Err(CcError::UnsupportedOption(
                "stop 停止序列（上游 /alpha/generate 不支持）".to_string(),
            ));
        }
    }

    let messages: &[Value] = match object.get("messages") {
        None | Some(Value::Null) => &[],
        Some(Value::Array(items)) => items,
        Some(_) => return Err(CcError::Protocol("messages 必须是数组".to_string())),
    };

    let tool_names = build_tool_name_index(messages);
    let converted_messages: Vec<Value> = messages
        .iter()
        .filter_map(|message| convert_message(message, &tool_names))
        .collect();

    let mut params = Map::new();
    params.insert(
        "model".to_string(),
        Value::String(
            object
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
                .unwrap_or(DEFAULT_MODEL)
                .to_string(),
        ),
    );
    params.insert("messages".to_string(), Value::Array(converted_messages));
    params.insert("max_tokens".to_string(), json!(resolve_max_tokens(object)?));
    // 上游 /alpha/generate 只有流式；对外是否流式由本地代理决定
    params.insert("stream".to_string(), Value::Bool(true));

    // PROTOCOL.md #1 / #3：system 恒为字符串；无 system 时用空格占位，避免上游注入
    // 约 7.5K token 的默认提示词（issue #17）。
    match extract_system_prompt(messages) {
        Some(system) if !system.is_empty() => {
            params.insert("system".to_string(), Value::String(system));
        }
        _ if cfg.empty_system_placeholder => {
            params.insert(
                "system".to_string(),
                Value::String(EMPTY_SYSTEM_PLACEHOLDER.to_string()),
            );
        }
        _ => {}
    }

    if let Some(temperature) = object.get("temperature") {
        match temperature {
            Value::Null => {}
            Value::Number(number) => {
                params.insert("temperature".to_string(), Value::Number(number.clone()));
            }
            _ => {
                return Err(CcError::UnsupportedOption(
                    "temperature 必须是数字".to_string(),
                ))
            }
        }
    }

    // reasoning_effort 不做值域白名单：上游的档位会漂移（low..max），白名单会把新档位
    // 误挡成客户端错误。
    if let Some(effort) = optional_string(object, "reasoning_effort")? {
        params.insert("reasoning_effort".to_string(), Value::String(effort));
    }

    if let Some(tools) = convert_tools(object.get("tools"))? {
        params.insert("tools".to_string(), Value::Array(tools));
    }

    if let Some(tool_choice) = convert_tool_choice(object.get("tool_choice"))? {
        params.insert("tool_choice".to_string(), tool_choice);
    }

    if let Some(parallel) = object.get("parallel_tool_calls") {
        match parallel {
            Value::Null => {}
            Value::Bool(flag) => {
                params.insert("parallel_tool_calls".to_string(), Value::Bool(*flag));
            }
            _ => {
                return Err(CcError::UnsupportedOption(
                    "parallel_tool_calls 必须是布尔值".to_string(),
                ))
            }
        }
    }

    Ok(json!({
        "config": {
            "workingDir": context.working_dir,
            "date": context.date,
            "environment": context.environment,
            "structure": context.structure,
            "isGitRepo": context.is_git_repo,
            "currentBranch": context.current_branch,
            "mainBranch": context.main_branch,
            "gitStatus": context.git_status,
            "recentCommits": context.recent_commits,
        },
        "memory": Value::Null,
        "taste": Value::Null,
        "skills": "",
        "permissionMode": "standard",
        "params": Value::Object(params),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 构造一个只有一个 user 消息的最小请求，减少各用例的样板。
    fn request_with(messages: Value) -> Value {
        json!({ "model": "deepseek/deepseek-v4-flash", "messages": messages })
    }

    #[test]
    fn system_array_content_is_flattened_into_a_string() {
        // 这是最容易回退的坑：把数组直接塞进 params.system
        let req = json!({
            "model": "m",
            "messages": [
                {
                    "role": "system",
                    "content": [
                        { "type": "text", "text": "第一段" },
                        { "type": "text", "text": "第二段" }
                    ]
                },
                { "role": "user", "content": "hi" }
            ]
        });
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert!(
            body["params"]["system"].is_string(),
            "PROTOCOL.md #1：params.system 必须是字符串，数组会被上游校验拒绝"
        );
        assert_eq!(
            body["params"]["system"], "第一段\n第二段",
            "数组型 content 必须展开取 text 再用换行拼接"
        );
        let serialized = serde_json::to_string(&body).expect("序列化不应失败");
        assert!(
            serialized.contains(r#""system":"第一段\n第二段""#),
            "序列化后的 system 也应是一个 JSON 字符串而不是块数组"
        );
    }

    #[test]
    fn multiple_system_and_developer_messages_are_joined_in_order() {
        let req = request_with(json!([
            { "role": "system", "content": "第一条" },
            { "role": "developer", "content": [{ "type": "text", "text": "第二条" }] },
            { "role": "user", "content": "hi" }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["system"], "第一条\n第二条",
            "system 与 developer 都应算系统提示，并按出现顺序拼接"
        );
    }

    #[test]
    fn extract_system_prompt_is_none_without_system_messages() {
        let messages = vec![json!({ "role": "user", "content": "hi" })];
        assert!(
            extract_system_prompt(&messages).is_none(),
            "没有系统消息时应返回 None，调用方据此决定是否发送空格占位"
        );
    }

    #[test]
    fn missing_system_sends_a_single_space_placeholder() {
        let req = request_with(json!([{ "role": "user", "content": "hi" }]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["system"], " ",
            "PROTOCOL.md #3：无 system 时必须发一个空格，否则上游注入约 7.5K token 默认提示词"
        );
    }

    #[test]
    fn empty_system_placeholder_can_be_disabled() {
        let cfg = Config {
            empty_system_placeholder: false,
            ..Config::default()
        };
        let req = request_with(json!([{ "role": "user", "content": "hi" }]));
        let body = build_generate_body(&req, &cfg).expect("合法请求应转换成功");
        assert!(
            body["params"].get("system").is_none(),
            "关闭占位开关后不应发送 system 字段，回到上游原生缺省行为"
        );
    }

    #[test]
    fn empty_system_message_also_falls_back_to_the_placeholder() {
        // 参考实现用 JS 的 falsy 判断：空字符串与「没有 system」走同一条占位分支
        let req = request_with(json!([
            { "role": "system", "content": "" },
            { "role": "user", "content": "hi" }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["system"], " ",
            "system 存在但内容为空时，参考实现同样回落到空格占位（falsy 语义）"
        );

        let cfg = Config {
            empty_system_placeholder: false,
            ..Config::default()
        };
        let body = build_generate_body(&req, &cfg).expect("合法请求应转换成功");
        assert!(
            body["params"].get("system").is_none(),
            "关闭占位后空 system 不发送，回到上游原生缺省行为"
        );
    }

    #[test]
    fn reasoning_block_comes_first_then_text_then_tool_call() {
        let req = request_with(json!([
            { "role": "user", "content": "读一下文件" },
            {
                "role": "assistant",
                "reasoning_content": "我需要先读文件",
                "content": "好的",
                "tool_calls": [
                    {
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read", "arguments": "{\"path\":\"/tmp/a\"}" }
                    }
                ]
            }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let content = body["params"]["messages"][1]["content"]
            .as_array()
            .expect("assistant 的 content 必须是数组");
        assert_eq!(content.len(), 3, "reasoning + text + tool-call 三个块");
        assert_eq!(
            content[0]["type"], "reasoning",
            "PROTOCOL.md #2：reasoning 必须排在内容块数组最前，顺序错了上游会拒绝"
        );
        assert_eq!(content[0]["text"], "我需要先读文件");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[2]["type"], "tool-call");
        assert_eq!(content[2]["toolCallId"], "call_1");
        assert_eq!(content[2]["toolName"], "read");
        assert_eq!(
            content[2]["input"]["path"], "/tmp/a",
            "arguments 字符串应被解析成对象"
        );
    }

    #[test]
    fn reasoning_from_content_array_is_forwarded_when_field_is_absent() {
        let req = request_with(json!([
            {
                "role": "assistant",
                "content": [
                    { "type": "reasoning", "text": "数组里的思考" },
                    { "type": "text", "text": "答案" }
                ]
            }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let content = body["params"]["messages"][0]["content"]
            .as_array()
            .expect("assistant 的 content 必须是数组");
        assert_eq!(
            content[0]["type"], "reasoning",
            "数组里的 reasoning 也要被提升到最前"
        );
        assert_eq!(content[0]["text"], "数组里的思考");
    }

    #[test]
    fn reasoning_is_not_duplicated_when_both_forms_are_present() {
        let req = request_with(json!([
            {
                "role": "assistant",
                "reasoning_content": "字段里的思考",
                "content": [
                    { "type": "reasoning", "text": "数组里的思考" },
                    { "type": "text", "text": "答案" }
                ]
            }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let content = body["params"]["messages"][0]["content"]
            .as_array()
            .expect("assistant 的 content 必须是数组");
        assert_eq!(content.len(), 2, "reasoning 只应出现一次");
        assert_eq!(content[0]["text"], "字段里的思考", "字段形式优先于数组形式");
    }

    #[test]
    fn tool_message_looks_up_its_name_from_previous_assistant_tool_calls() {
        let req = request_with(json!([
            { "role": "user", "content": "读一下" },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    { "id": "call_9", "type": "function", "function": { "name": "read_file", "arguments": "{}" } }
                ]
            },
            { "role": "tool", "tool_call_id": "call_9", "content": "文件内容" }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let result = &body["params"]["messages"][2]["content"][0];
        assert_eq!(body["params"]["messages"][2]["role"], "tool");
        assert_eq!(result["type"], "tool-result");
        assert_eq!(result["toolCallId"], "call_9");
        assert_eq!(
            result["toolName"], "read_file",
            "OpenAI 的 tool 消息不带工具名，必须回查 assistant 的 tool_calls"
        );
        assert_eq!(result["output"]["type"], "text");
        assert_eq!(result["output"]["value"], "文件内容");
    }

    #[test]
    fn tool_message_falls_back_to_unknown_when_the_call_is_missing() {
        let req = request_with(json!([
            { "role": "user", "content": "hi" },
            { "role": "tool", "tool_call_id": "call_missing", "content": "结果" }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let result = &body["params"]["messages"][1]["content"][0];
        assert_eq!(
            result["toolName"], "unknown",
            "查不到配对 tool_call 时应给出可读占位名，而不是空串"
        );
    }

    #[test]
    fn tool_message_prefers_the_index_over_its_own_name_field() {
        let req = request_with(json!([
            {
                "role": "assistant",
                "content": "调用",
                "tool_calls": [
                    { "id": "c1", "type": "function", "function": { "name": "indexed", "arguments": "{}" } }
                ]
            },
            { "role": "tool", "tool_call_id": "c1", "name": "own_name", "content": "x" }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["messages"][1]["content"][0]["toolName"], "indexed",
            "查表结果优先于消息自带的 name，与参考实现一致"
        );
    }

    #[test]
    fn tool_choice_strings_map_to_the_anthropic_types() {
        let cases = [("auto", "auto"), ("none", "none"), ("required", "any")];
        for (input, expected) in cases {
            let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
            req["tool_choice"] = json!(input);
            let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
            assert_eq!(
                body["params"]["tool_choice"]["type"], expected,
                "OpenAI 的 {input} 应映射为 CC 的 {expected}"
            );
        }
    }

    #[test]
    fn unknown_tool_choice_string_falls_back_to_auto() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["tool_choice"] = json!("brand-new-mode");
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["tool_choice"]["type"], "auto",
            "未知取值按参考实现退化为 auto，而不是把请求打回"
        );
    }

    #[test]
    fn named_tool_choice_maps_to_tool_with_name() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["tool_choice"] = json!({ "type": "function", "function": { "name": "read" } });
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["tool_choice"],
            json!({ "type": "tool", "name": "read" })
        );
    }

    #[test]
    fn named_tool_choice_without_a_name_is_rejected() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["tool_choice"] = json!({ "type": "function", "function": {} });
        let err = build_generate_body(&req, &Config::default()).expect_err("缺少工具名应报错");
        assert!(
            matches!(err, CcError::Protocol(_)),
            "指定了没有名字的工具属于非法请求，应返回 CcError::Protocol"
        );
    }

    #[test]
    fn anthropic_shaped_tool_choice_is_passed_through() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["tool_choice"] = json!({ "type": "any" });
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["tool_choice"],
            json!({ "type": "any" }),
            "已经是 CC / Anthropic 形状的对象应原样透传"
        );
    }

    #[test]
    fn empty_user_messages_are_dropped() {
        let req = request_with(json!([
            { "role": "user", "content": "" },
            { "role": "user", "content": [] },
            { "role": "user", "content": [{ "type": "text", "text": "" }] },
            { "role": "user", "content": [{ "type": "input_audio", "input_audio": { "data": "x" } }] },
            { "role": "user", "content": "真的内容" }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let messages = body["params"]["messages"]
            .as_array()
            .expect("messages 必须是数组");
        assert_eq!(
            messages.len(),
            1,
            "转换后没有任何内容块的 user 消息应被丢弃"
        );
        assert_eq!(messages[0]["content"][0]["text"], "真的内容");
    }

    #[test]
    fn unknown_roles_are_normalized_to_user_with_array_content() {
        // PROTOCOL.md #6：未知 role 会被上游校验拒绝，必须归一化
        let req = request_with(json!([
            { "role": "function", "content": "legacy tool output" },
            { "content": "没有 role" }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let messages = body["params"]["messages"]
            .as_array()
            .expect("messages 必须是数组");
        assert_eq!(messages.len(), 2, "未知 role 应被保留并归一化，而不是丢弃");
        for message in messages {
            assert_eq!(message["role"], "user", "未知 role 一律归一化为 user");
            assert!(message["content"].is_array(), "归一化后 content 必须是数组");
        }
        assert_eq!(messages[1]["content"][0]["text"], "没有 role");
    }

    #[test]
    fn image_url_parts_become_cc_image_parts() {
        let req = request_with(json!([{
            "role": "user",
            "content": [
                { "type": "text", "text": "这是什么" },
                { "type": "image_url", "image_url": { "url": "data:image/jpeg;base64,AAAA" } }
            ]
        }]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let content = body["params"]["messages"][0]["content"]
            .as_array()
            .expect("user 的 content 必须是数组");
        assert_eq!(content.len(), 2, "文本与图片都应保留");
        assert_eq!(content[1]["type"], "image");
        assert_eq!(content[1]["image"], "data:image/jpeg;base64,AAAA");
    }

    #[test]
    fn tool_definitions_are_flattened_into_the_cc_shape() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["tools"] = json!([
            {
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "读文件",
                    "parameters": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            },
            { "type": "function", "function": { "name": "no_schema" } }
        ]);
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let tools = body["params"]["tools"]
            .as_array()
            .expect("tools 必须是数组");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "read", "工具名应被拍平到顶层");
        assert_eq!(tools[0]["description"], "读文件");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
        assert_eq!(
            tools[1]["input_schema"],
            json!({ "type": "object", "properties": {} }),
            "PROTOCOL.md #7：缺省 schema 必须是合法的 object 根，否则整轮失败"
        );
    }

    #[test]
    fn empty_tool_list_is_not_sent() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["tools"] = json!([]);
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert!(
            body["params"].get("tools").is_none(),
            "空工具数组不发送，避免上游把它当成「本轮禁用工具」"
        );
    }

    #[test]
    fn malformed_tool_arguments_degrade_to_an_empty_object() {
        let req = request_with(json!([{
            "role": "assistant",
            "content": "调用",
            "tool_calls": [
                { "id": "c1", "type": "function", "function": { "name": "a", "arguments": "{不是 JSON" } },
                { "id": "c2", "type": "function", "function": { "name": "b", "arguments": "" } },
                { "id": "c3", "type": "function", "function": { "name": "c", "arguments": { "ok": true } } }
            ]
        }]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let content = body["params"]["messages"][0]["content"]
            .as_array()
            .expect("assistant 的 content 必须是数组");
        assert_eq!(content[0]["type"], "text", "文本块排在 tool-call 之前");
        assert_eq!(
            content[1]["input"],
            json!({}),
            "坏 JSON 应退化为空对象而不是让整条请求失败"
        );
        assert_eq!(content[2]["input"], json!({}), "空字符串等价于无参数");
        assert_eq!(
            content[3]["input"],
            json!({ "ok": true }),
            "已解析的对象应原样透传"
        );
    }

    #[test]
    fn tool_call_without_an_id_is_dropped() {
        let req = request_with(json!([{
            "role": "assistant",
            "content": "调用",
            "tool_calls": [
                { "type": "function", "function": { "name": "a", "arguments": "{}" } },
                { "id": "c2", "type": "function", "function": { "name": "b", "arguments": "{}" } }
            ]
        }]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let content = body["params"]["messages"][0]["content"]
            .as_array()
            .expect("assistant 的 content 必须是数组");
        assert_eq!(
            content.len(),
            2,
            "PROTOCOL.md #5：没有 id 的 tool-call 无法与 tool-result 配对，必须丢弃"
        );
        assert_eq!(content[1]["toolCallId"], "c2");
    }

    #[test]
    fn max_tokens_is_capped_and_defaulted_like_the_reference() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["max_tokens"], 64_000,
            "未提供 max_tokens 时的默认值"
        );

        req["max_tokens"] = json!(200_000);
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["max_tokens"], 200_000,
            "恰好等于上限时应原样保留"
        );

        req["max_tokens"] = json!(200_001);
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["max_tokens"], 200_000,
            "超过上限应被夹紧而不是报错"
        );

        req["max_tokens"] = json!(0);
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["max_tokens"], 64_000,
            "0 与「未提供」同义（参考实现的 || 语义）"
        );
    }

    #[test]
    fn stop_sequences_are_rejected_not_silently_dropped() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["stop"] = json!(["END"]);
        let err = build_generate_body(&req, &Config::default()).expect_err("stop 应被拒绝");
        assert!(
            matches!(err, CcError::UnsupportedOption(_)),
            "PROTOCOL.md #12：上游不支持 stop，必须显式报错而不是静默丢弃"
        );
    }

    #[test]
    fn stream_is_always_true_and_pass_through_options_are_forwarded() {
        let mut req = request_with(json!([{ "role": "user", "content": "hi" }]));
        req["stream"] = json!(false);
        req["temperature"] = json!(0.3);
        req["reasoning_effort"] = json!("high");
        req["parallel_tool_calls"] = json!(false);
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert_eq!(
            body["params"]["stream"], true,
            "/alpha/generate 只有流式；对外是否流式由本地代理缓冲决定"
        );
        assert_eq!(body["params"]["temperature"], 0.3);
        assert_eq!(body["params"]["reasoning_effort"], "high");
        assert_eq!(body["params"]["parallel_tool_calls"], false);
    }

    #[test]
    fn top_level_shape_matches_the_generate_endpoint() {
        let req = request_with(json!([{ "role": "user", "content": "hi" }]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        assert!(
            body["memory"].is_null(),
            "PROTOCOL.md 第 2 节：memory 恒为 null"
        );
        assert!(body["taste"].is_null(), "taste 恒为 null");
        assert_eq!(body["skills"], "", "skills 恒为空字符串");
        assert_eq!(body["permissionMode"], "standard");
        assert_eq!(body["config"]["structure"], json!([]));
        assert_eq!(body["config"]["isGitRepo"], false);
        assert_eq!(body["config"]["recentCommits"], json!([]));
    }

    #[test]
    fn injected_environment_snapshot_lands_in_the_config_block() {
        let context = GenerateContext {
            working_dir: "/home/dev/app".to_string(),
            date: "2024-05-06".to_string(),
            environment: "darwin-arm64".to_string(),
            structure: vec!["src".to_string()],
            is_git_repo: true,
            current_branch: "feat/x".to_string(),
            main_branch: "main".to_string(),
            git_status: " M src/lib.rs".to_string(),
            recent_commits: vec!["abc123 init".to_string()],
        };
        let req = request_with(json!([{ "role": "user", "content": "hi" }]));
        let body = build_generate_body_with_context(&req, &Config::default(), &context)
            .expect("合法请求应转换成功");
        assert_eq!(
            body["config"]["workingDir"], "/home/dev/app",
            "环境快照只能由参数注入，模块内部不读系统时间与磁盘"
        );
        assert_eq!(body["config"]["date"], "2024-05-06");
        assert_eq!(body["config"]["isGitRepo"], true);
        assert_eq!(body["config"]["currentBranch"], "feat/x");
        assert_eq!(body["config"]["recentCommits"][0], "abc123 init");
    }

    #[test]
    fn malformed_request_bodies_are_rejected_without_panicking() {
        let cfg = Config::default();
        assert!(
            matches!(
                build_generate_body(&json!("不是对象"), &cfg),
                Err(CcError::Protocol(_))
            ),
            "请求体不是 JSON 对象时应报协议错误"
        );
        assert!(
            matches!(
                build_generate_body(&json!({ "messages": "不是数组" }), &cfg),
                Err(CcError::Protocol(_))
            ),
            "messages 不是数组时应报协议错误"
        );
        assert!(
            matches!(
                build_generate_body(&json!({ "messages": [], "tools": {} }), &cfg),
                Err(CcError::Protocol(_))
            ),
            "tools 不是数组时应报协议错误"
        );
        let mut bad_effort = json!({ "messages": [] });
        bad_effort["reasoning_effort"] = json!(3);
        assert!(
            matches!(
                build_generate_body(&bad_effort, &cfg),
                Err(CcError::UnsupportedOption(_))
            ),
            "reasoning_effort 类型不对应报不支持选项，而不是静默忽略"
        );
        let mut bad_temperature = json!({ "messages": [] });
        bad_temperature["temperature"] = json!("热");
        assert!(
            matches!(
                build_generate_body(&bad_temperature, &cfg),
                Err(CcError::UnsupportedOption(_))
            ),
            "temperature 类型不对应报不支持选项"
        );
        let mut bad_parallel = json!({ "messages": [] });
        bad_parallel["parallel_tool_calls"] = json!("yes");
        assert!(
            matches!(
                build_generate_body(&bad_parallel, &cfg),
                Err(CcError::UnsupportedOption(_))
            ),
            "parallel_tool_calls 类型不对应报不支持选项"
        );
    }

    #[test]
    fn empty_messages_still_produce_a_well_formed_body() {
        let req = json!({ "messages": [] });
        let body = build_generate_body(&req, &Config::default()).expect("空消息列表应能转换");
        assert_eq!(body["params"]["messages"], json!([]));
        assert_eq!(
            body["params"]["model"], "deepseek/deepseek-v4-flash",
            "模型缺省时应兜底"
        );
        assert_eq!(
            body["params"]["system"], " ",
            "没有 system 时同样要发空格占位"
        );
    }

    #[test]
    fn assistant_message_without_any_content_is_dropped() {
        let req = request_with(json!([
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": null }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let messages = body["params"]["messages"]
            .as_array()
            .expect("messages 必须是数组");
        assert_eq!(
            messages.len(),
            1,
            "既无文本也无工具调用的 assistant 消息应被丢弃"
        );
    }

    #[test]
    fn tool_message_without_content_becomes_an_empty_result() {
        let req = request_with(json!([
            {
                "role": "assistant",
                "content": "调用",
                "tool_calls": [
                    { "id": "c1", "type": "function", "function": { "name": "t", "arguments": "{}" } }
                ]
            },
            { "role": "tool", "tool_call_id": "c1" }
        ]));
        let body = build_generate_body(&req, &Config::default()).expect("合法请求应转换成功");
        let result = &body["params"]["messages"][1]["content"][0];
        assert_eq!(
            result["output"]["value"], "",
            "PROTOCOL.md #5：即使结果为空也必须保留配对的 tool-result"
        );
        assert_eq!(result["toolName"], "t");
    }
}
