//! **意图分类路由**：用一个小模型判断「这条消息有多难」，据此选模型。
//!
//! ── 它解决什么问题 ────────────────────────────────────────────
//! 同一个对外模型名背后可以有多档能力（快模型 / 均衡模型 / 深度模型）。
//! 客户端点一个名字过来，网关只能按静态规则选 —— 于是「你好」和「重构整个
//! 调度器」用了同一个重模型，前者白花钱，后者不够强。
//!
//! 意图分类让网关**先问一句**「这条消息有多难」，再据此收窄候选：
//!
//! ```text
//!   客户端请求 ──► 意图分类器（小模型，只问一次）
//!                      │
//!        ┌─────────────┼─────────────┐
//!        ▼             ▼             ▼
//!     简单档        中等档        困难档
//!     flash         pro           max
//! ```
//!
//! ── 参照 magpie 的做法与差异 ──────────────────────────────────
//! magpie（`internal/gateway/classify.go`）在回合开始时问一次分类器，
//! 拿到 **intent**（属于哪类）与 **effort**（有多难），用 effort 去调
//! `reasoning_effort`；intent 用来匹配带意图的路由规则。
//!
//! 本项目的落点不同：**用「选成员」代替「调 effort」**。理由是本项目的
//! 路由组成员各自绑定了不同的上游模型（快/中/慢各有各的模型），而
//! `reasoning_effort` 只有部分厂商认（`fitFor` 那套映射在 magpie 里占了
//! 好大一段）。「选成员」是本项目结构下最自然、也最省事的落地方式。
//!
//! 提示词借鉴 magpie 的两条经验：
//!   - **不指名 assistant 的类型**。magpie 的注释记了这个教训：说成「编码助手」
//!     会让分类器拿编码的标准去评判闲聊/翻译。
//!   - **只让模型输出编号**，不要求它输出理由 —— 理由要花 token 且容易跑偏。
//!
//! ── 三条硬约束 ────────────────────────────────────────────────
//!   1. **分类器是优化，不是依赖**：它失败/超时/答非所问，一律**不分类**，
//!      按原候选集合走。绝不因为分类器坏了而让用户请求失败。
//!   2. **回合内只问一次**：分类结果按消息内容缓存（默认 10 分钟），
//!      同一句话不会反复问分类器。
//!   3. **不污染用户请求的账**：分类器调用走独立通道，不计入用户的 usage、
//!      不触发用户请求的冷却统计。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::server::config::routing_groups::{IntentClassifier, IntentLevel};

/// 内部字段：分类器判出的档位 id（api 层写，转发层读）。
///
/// 与 `_wb_route_group_id` 同一套约定 —— 内部字段进请求体，转发层消费，
/// 发给上游前会被剥掉（见 `api::request_prepare`）。
pub const FIELD_LEVEL: &str = "_wb_intent_level";

/// 内部字段：分类失败的原因标签（api 层写，转发层读；仅用于排查）。
pub const FIELD_FAILED: &str = "_wb_intent_failed";

/// 内部字段：分类请求要用的**组内成员 id**。
///
/// 分类请求走路由组，但候选要收窄到「分类器那个成员」的**全部**账号 ——
/// 钉死单个账号（`_wb_route_continue`）会挑到那个恰好在冷却的账号，
/// 于是分类器自己被挡在门外（实测踩过：清冷却清了 3 个账号，分类请求却挑中
/// 另一个还在冷却的）。按成员过滤则哪个账号能用就用哪个。
pub const FIELD_MEMBER: &str = "_wb_intent_member";

/// 分类结果：命中的档位下标（0 起）与档位 id。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentVerdict {
    /// 命中的档位在 `IntentClassifier.levels` 里的下标
    pub level_index: usize,
    /// 命中的档位 id
    pub level_id: String,
    /// 分类器原始回答（排查用：可能是「不是任何一档」）
    pub said: String,
}

/// 分类失败的原因（会进请求日志的路由快照里，便于排查「为什么没按档位走」）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentFailure {
    /// 没配意图分类
    NotConfigured,
    /// 请求里没有可分类的用户文本
    NoUserText,
    /// 分类器调用失败（网络/上游错误）
    Upstream(String),
    /// 分类器超时
    Timeout,
    /// 回答不是可识别的编号
    Unreadable(String),
}

impl IntentFailure {
    /// 机器可读的标签（进日志）。
    pub fn tag(&self) -> &'static str {
        match self {
            IntentFailure::NotConfigured => "not-configured",
            IntentFailure::NoUserText => "no-user-text",
            IntentFailure::Upstream(_) => "upstream-error",
            IntentFailure::Timeout => "timeout",
            IntentFailure::Unreadable(_) => "unreadable",
        }
    }

    /// 人能读的细节（进日志）—— 光有标签不够：线上要能判断是「模型不可用」
    /// 还是「上游报错」还是「回答读不出来」。
    pub fn detail(&self) -> String {
        match self {
            IntentFailure::NotConfigured => "未配置意图分类".to_string(),
            IntentFailure::NoUserText => "请求里没有可分类的用户文本".to_string(),
            IntentFailure::Upstream(message) => message.clone(),
            IntentFailure::Timeout => "分类器超时".to_string(),
            IntentFailure::Unreadable(message) => format!("回答读不出来：{message}"),
        }
    }
}

/// 缓存的一条分类结果。
#[derive(Clone, Debug)]
struct Cached {
    verdict: IntentVerdict,
    /// 过期时刻（毫秒）
    expires_at: i64,
}

/// 分类结果缓存。
///
/// 键 = 「分类器模型 + 消息摘要 + 档位集合」的哈希 —— 同一句话在同一套档位下
/// 只问一次。值带 TTL（默认 10 分钟，与 magpie 的 `classifyKeep` 同量级）。
fn cache() -> &'static Mutex<HashMap<String, Cached>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 缓存键：分类器 + 消息 + 档位集合。
///
/// 档位集合也进键：改了档位描述就得重新分类（旧的分类结果对新档位没有意义）。
pub fn cache_key(classifier: &IntentClassifier, text: &str) -> String {
    let levels: Vec<String> = classifier.levels.iter()
        .map(|level| format!("{}={}", level.id, level.label)).collect();
    let material = format!("{}\u{0}{}", classifier.classifier_member, levels.join("\u{1}"));
    format!("{:x}:{:x}",
        Sha256::digest(material.as_bytes()),
        Sha256::digest(text.as_bytes()))
}

/// 读缓存（命中且未过期才返回）。
pub fn cached(classifier: &IntentClassifier, text: &str, now_ms: i64) -> Option<IntentVerdict> {
    let key = cache_key(classifier, text);
    let mut table = cache().lock().ok()?;
    let entry = table.get(&key)?;
    if entry.expires_at <= now_ms {
        table.remove(&key);
        return None;
    }
    Some(entry.verdict.clone())
}

/// 写缓存。`cache_ms == 0` 时不写（调用方也可以直接不调）。
pub fn remember(classifier: &IntentClassifier, text: &str, verdict: IntentVerdict, now_ms: i64) {
    if classifier.cache_ms == 0 {
        return;
    }
    let key = cache_key(classifier, text);
    let Ok(mut table) = cache().lock() else { return };
    // 表上限：防内存无限增长（正常部署远达不到）
    if table.len() > 4096 {
        table.clear();
    }
    table.insert(key, Cached { verdict, expires_at: now_ms + classifier.cache_ms as i64 });
}

/// 构造给分类器看的**用户消息**（即「被分类的那段话」）。
///
/// 只取**最后一条 user 消息** —— 分类的是「这一轮」，不是整段历史。
/// magpie 也是这个口径（`text` 是当前回合）。
pub fn user_text(body: &Value) -> Option<String> {
    let messages = body.get("messages")?.as_array()?;
    let mut last: Option<&Value> = None;
    for message in messages {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "user" {
            last = Some(message);
        }
    }
    let message = last?;
    // 文本可能是字符串，也可能是多模态数组（content parts）
    match message.get("content") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(parts)) => {
            let mut joined = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !joined.is_empty() { joined.push('\n'); }
                    joined.push_str(text);
                }
            }
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

/// 构造分类提示词。
///
/// ── 为什么不指名 assistant 的类型 ──────────────────────────────
/// magpie 的注释记了这个教训（`classifyPrompt`）：把分类说成「编码助手」会让
/// 分类器拿编码的标准去评判闲聊、故事、翻译 —— 判出来的档位全偏。
/// 所以这里只说「一条发给 AI 助手的用户消息」，让它按**内容难易**分档。
///
/// ── 为什么只输出编号 ─────────────────────────────────────────
/// 让模型输出理由会多花 token，而且理由容易跑偏（说了些不相关内容，编号反而
/// 跑掉）。只要编号 —— 解析失败就当「不分类」。
pub fn classify_prompt(levels: &[IntentLevel]) -> String {
    let mut prompt = String::from(
        "You sort a user's message, sent to an AI assistant, into one of several levels of difficulty. ",
    );
    prompt.push_str(
        "Given numbered levels and the user's message, answer with the number of the level that fits the message best. ",
    );
    prompt.push_str(
        "The levels may be topics, or levels of how hard or how big a request is; ",
    );
    prompt.push_str(
        "when they are levels, every message has one, a greeting or a question included. ",
    );
    prompt.push_str("Answer 0 only when the message is plainly none of the levels. Answer with the number only.",
    );
    prompt
}

/// 构造分类请求的用户消息体：编号化的档位 + 被分类的文本。
pub fn classify_body(levels: &[IntentLevel], text: &str, max_chars: usize) -> String {
    let mut body = String::from("Levels:\n");
    for (index, level) in levels.iter().enumerate() {
        body.push_str(&format!("{}. {}\n", index + 1, level.label));
    }
    body.push_str("\nUser's message:\n");
    // 截断：分类只需要看个大概，几千字足够判断难易了 —— 而分类器往往是最贵的那一跳
    let truncated: String = text.chars().take(max_chars).collect();
    body.push_str(&truncated);
    if text.chars().count() > max_chars {
        body.push_str("\n… (truncated)");
    }
    body
}

/// 从分类器的回答里读出档位下标。
///
/// 只认**纯数字**（1 起）。模型爱加解释（「2，因为这个任务…」），所以：
/// - 纯数字 → 直接读；
/// - 开头是数字后跟别的东西 → 取开头那个数字（宽松，但比解析失败强）；
/// - 找不到数字 → `None`（当作「不分类」）。
///
/// magpie 的 `readIntent` 也是这个思路（读一个数字，别的一概不取）。
pub fn parse_verdict(answer: &str, levels: &[IntentLevel]) -> Option<IntentVerdict> {
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        return None;
    }
    // 取开头的连续数字
    let digits: String = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let Ok(number) = digits.parse::<usize>() else { return None };
    if number == 0 {
        // 0 = 「都不是」→ 不分类（走原候选）
        return None;
    }
    let index = number.checked_sub(1)?;
    let level = levels.get(index)?;
    Some(IntentVerdict {
        level_index: index,
        level_id: level.id.clone(),
        said: trimmed.chars().take(120).collect(),
    })
}

/// 命中的档位应当保留哪些成员 id。
///
/// 空列表 = 这一档不做收窄（等价于「都行」）。
pub fn allowed_members(level: &IntentLevel) -> &[String] {
    &level.members
}

/// 分类器请求的**对外**形态：只发最少的字段，不带用户的任何内容之外的东西。
///
/// 刻意**不带** `stream`（要的是一段完整回答，不是 SSE）。
///
/// `model` 用**组的对外模型名** + `_wb_route_group_id` / `_wb_route_continue`
/// —— 这样这次请求走**组内**的候选（而不是按模型名去全局队列找承载家），
/// 且被钉在分类器那个成员上。理由见 `IntentClassifier` 的注释。
pub fn classifier_request_body(
    classifier: &IntentClassifier,
    group_model: &str,
    group_id: &str,
    text: &str,
) -> Value {
    json!({
        "model": group_model,
        "messages": [
            { "role": "system", "content": classify_prompt(&classifier.levels) },
            { "role": "user", "content": classify_body(&classifier.levels, text, 4000) },
        ],
        "stream": false,
        "temperature": 0,
        // 2048（与 magpie 一致）：有些模型会**先输出一段思考**再给答案，
        // max_tokens 给太小会把编号截掉，答案就读不出来了。
        "max_tokens": 2048,
        "_wb_route_group_id": group_id,
        // 收窄到分类器那个成员的**全部**账号（见 FIELD_MEMBER 的说明）
        "_wb_intent_member": classifier.classifier_member,
    })
}

/// 从上游回答里取出文本内容（兼容 OpenAI 风格的 `choices[0].message.content`）。
pub fn answer_text(body: &Value) -> Option<String> {
    let choice = body.get("choices")?.as_array()?.first()?;
    let content = choice.get("message").and_then(|message| message.get("content"))
        .or_else(|| choice.get("text"))?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut joined = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    joined.push_str(text);
                }
            }
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

/// 分类器调用的**执行器**：把一次分类真正跑起来。
///
/// ── 为什么走完整的转发链 ──────────────────────────────────────
/// 上游各家（插件）的请求构造、鉴权、代理、协议适配各不相同 —— 复刻一遍必然
/// 与生产链路口径不一。走同一条链（`service.forward`）意味着「分类器用的
/// 正是用户请求会用的那套通路」。magpie 也是这么做的（"The call goes through
/// magpie itself"）。
///
/// ── 副作用隔离 ────────────────────────────────────────────────
/// - **usage**：记账在 api 层（`pipeline::record_entry`），转发层不写 usage ——
///   这里不调它，分类器的 token 不会算到用户头上。
/// - **telemetry**：用**独立**的 `RequestTelemetry`，不与用户请求混。
/// - **冷却/评分**：分类器调用仍会经过 RouteState（它也要被冷却保护）——
///   这是**对的**：分类器模型挂了就该被冷却，不该每请求都去撞它。
/// - **递归**：请求带 `intent_call: true`，分类器自己的路由组不再触发分类。
pub async fn classify(
    service: &crate::server::core::upstream::UpstreamService,
    classifier: &IntentClassifier,
    group: &crate::server::config::RoutingGroup,
    text: &str,
) -> Result<IntentVerdict, IntentFailure> {
    // 先看缓存 —— 同一句话不问第二次
    let now = crate::server::logging::now_ms();
    if let Some(verdict) = cached(classifier, text, now) {
        return Ok(verdict);
    }
    if user_text_missing(text) {
        return Err(IntentFailure::NoUserText);
    }
    let body = classifier_request_body(classifier, &group.model, &group.id, text);
    let telemetry = std::sync::Arc::new(crate::server::core::upstream::usage::RequestTelemetry::new());
    let request = crate::server::core::upstream::ForwardRequest {
        body,
        stream: false,
        dedupe_key: String::new(),
        client_headers: axum::http::HeaderMap::new(),
        telemetry,
        // 不限定提供商：这次请求走**路由组**（body 里带了 `_wb_route_group_id`
        // 与 `_wb_route_continue`），候选来自组内成员 —— 分类器的开销算在组上，
        // 与这一轮的请求走同一批账号（同一套限制器规则）。
        allowed_providers: None,
        pinned_account: None,
        ignore_model_gate: false,
        proxy_override: None,
        intent_call: true,
    };
    // 超时：分类器是优化，不能让它拖住用户请求
    let timeout = std::time::Duration::from_millis(classifier.timeout_ms.max(1));
    let outcome = match tokio::time::timeout(timeout, service.forward(request)).await {
        Err(_) => return Err(IntentFailure::Timeout),
        Ok(Err(error)) => return Err(IntentFailure::Upstream(error.message)),
        Ok(Ok(outcome)) => outcome,
    };
    let crate::server::core::upstream::ForwardOutcome::Completion { body } = outcome else {
        // 流式结果（不该发生：分类请求显式 stream=false）—— 当作读不出来
        return Err(IntentFailure::Unreadable("分类器返回了流式结果".to_string()));
    };
    let Some(answer) = answer_text(&body) else {
        return Err(IntentFailure::Unreadable("分类器回答里没有文本".to_string()));
    };
    let Some(verdict) = parse_verdict(&answer, &classifier.levels) else {
        return Err(IntentFailure::Unreadable(answer.chars().take(120).collect()));
    };
    remember(classifier, text, verdict.clone(), now);
    Ok(verdict)
}

/// 内部用：文本为空时判为「没有可分类内容」。
fn user_text_missing(text: &str) -> bool {
    text.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(id: &str, label: &str, members: &[&str]) -> IntentLevel {
        IntentLevel {
            id: id.to_string(),
            label: label.to_string(),
            members: members.iter().map(|value| value.to_string()).collect(),
        }
    }

    fn classifier() -> IntentClassifier {
        IntentClassifier {
            classifier_member: "flash".to_string(),
            timeout_ms: 8000,
            levels: vec![
                level("easy", "a greeting or a one-line question", &["flash"]),
                level("hard", "a design task or a multi-file change", &["max"]),
            ],
            cache_ms: 600_000,
        }
    }

    /// 纯数字回答 → 命中对应档位。
    #[test]
    fn plain_number_is_read_as_the_level() {
        let levels = classifier().levels;
        let verdict = parse_verdict("2", &levels).expect("应当读出档位");
        assert_eq!(verdict.level_index, 1);
        assert_eq!(verdict.level_id, "hard");
    }

    /// 带解释的回答 → 仍取开头的编号（模型爱多说）。
    #[test]
    fn number_with_explanation_is_read() {
        let levels = classifier().levels;
        let verdict = parse_verdict("2, because it is a design task", &levels).expect("应当读出档位");
        assert_eq!(verdict.level_index, 1);
    }

    /// 「0」= 都不是 → 不分类（不是错误，是有效回答）。
    #[test]
    fn zero_means_none_and_yields_no_verdict() {
        assert_eq!(parse_verdict("0", &classifier().levels), None);
    }

    /// 答不出编号 → 不分类（降级到原候选）。
    #[test]
    fn unreadable_answer_yields_no_verdict() {
        assert_eq!(parse_verdict("I cannot tell", &classifier().levels), None);
        assert_eq!(parse_verdict("", &classifier().levels), None);
        assert_eq!(parse_verdict("99", &classifier().levels), None);
    }

    /// 提示词不指名 assistant 类型（magpie 的教训）。
    #[test]
    fn prompt_does_not_name_an_assistant_kind() {
        let prompt = classify_prompt(&classifier().levels);
        assert!(!prompt.to_lowercase().contains("coding"));
        assert!(!prompt.to_lowercase().contains("code assistant"));
        assert!(prompt.contains("number only"));
    }

    /// 档位描述要进提示词，编号要连号。
    #[test]
    fn body_lists_levels_with_sequential_numbers() {
        let body = classify_body(&classifier().levels, "hello", 4000);
        assert!(body.contains("1. a greeting or a one-line question"));
        assert!(body.contains("2. a design task or a multi-file change"));
        assert!(body.ends_with("hello"));
    }

    /// 超长文本要截断（分类器往往是最贵的一跳）。
    #[test]
    fn body_truncates_long_text() {
        let long = "x".repeat(5000);
        let body = classify_body(&classifier().levels, &long, 100);
        assert!(body.contains("(truncated)"));
        assert!(body.len() < 1000);
    }

    /// 只取最后一条 user 消息 —— 分类的是「这一轮」。
    #[test]
    fn only_the_last_user_message_is_classified() {
        let body = json!({ "messages": [
            { "role": "system", "content": "sys" },
            { "role": "user", "content": "first question" },
            { "role": "assistant", "content": "an answer" },
            { "role": "user", "content": "second question" },
        ]});
        assert_eq!(user_text(&body).as_deref(), Some("second question"));
    }

    /// 多模态消息（content 是数组）也能取出文本。
    #[test]
    fn multipart_content_is_joined() {
        let body = json!({ "messages": [
            { "role": "user", "content": [
                { "type": "text", "text": "look at this" },
                { "type": "image_url", "image_url": { "url": "http://x/y.png" } },
            ]},
        ]});
        assert_eq!(user_text(&body).as_deref(), Some("look at this"));
    }

    /// 没有 user 消息 → 无可分类内容。
    #[test]
    fn no_user_message_means_nothing_to_classify() {
        let body = json!({ "messages": [{ "role": "system", "content": "sys" }] });
        assert_eq!(user_text(&body), None);
    }

    /// 缓存：同一条消息命中；改了档位则键不同（要重新分类）。
    #[test]
    fn cache_key_changes_with_levels_and_text() {
        let one = classifier();
        let key_a = cache_key(&one, "hello");
        assert_eq!(key_a, cache_key(&one, "hello"));
        assert_ne!(key_a, cache_key(&one, "other"));
        let mut two = one.clone();
        two.levels[0].label = "something else".to_string();
        assert_ne!(key_a, cache_key(&two, "hello"));
    }

    /// 缓存按 TTL 过期。
    #[test]
    fn cached_entry_expires() {
        let one = classifier();
        let text = "cache me";
        let verdict = IntentVerdict { level_index: 0, level_id: "easy".to_string(), said: "1".to_string() };
        remember(&one, text, verdict.clone(), 1000);
        assert_eq!(cached(&one, text, 2000).map(|v| v.level_id), Some("easy".to_string()));
        // 过期后读不到
        assert_eq!(cached(&one, text, 1000 + one.cache_ms as i64 + 1), None);
    }

    /// 分类器请求体：带档位、只输出编号、不流式、钉在组内成员上。
    #[test]
    fn classifier_body_is_minimal_and_non_streaming() {
        let body = classifier_request_body(&classifier(), "my-group", "my-group", "hello");
        assert_eq!(body.get("model").and_then(Value::as_str), Some("my-group"));
        assert_eq!(body.get("_wb_route_group_id").and_then(Value::as_str), Some("my-group"));
        assert_eq!(body.get("stream").and_then(Value::as_bool), Some(false));
        assert_eq!(body.get("max_tokens").and_then(Value::as_u64), Some(2048));
        let messages = body.get("messages").and_then(Value::as_array).expect("messages");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].get("role").and_then(Value::as_str), Some("system"));
    }

    /// 分类请求带上 `_wb_intent_member`（收窄到分类器那个成员的全部账号）。
    #[test]
    fn classifier_body_names_the_member_to_use() {
        let body = classifier_request_body(&classifier(), "my-group", "my-group", "hello");
        assert_eq!(body.get("_wb_intent_member").and_then(Value::as_str), Some("flash"));
    }

    /// 从上游回答里读文本。
    #[test]
    fn answer_text_reads_openai_shape() {
        let body = json!({ "choices": [{ "message": { "content": "2" } }]});
        assert_eq!(answer_text(&body).as_deref(), Some("2"));
        let parts = json!({ "choices": [{ "message": { "content": [{ "type": "text", "text": "1" }] } }]});
        assert_eq!(answer_text(&parts).as_deref(), Some("1"));
        assert_eq!(answer_text(&json!({})), None);
    }
}
