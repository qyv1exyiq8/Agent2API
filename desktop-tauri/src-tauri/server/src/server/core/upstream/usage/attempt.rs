use serde_json::Value;

use super::{truncate_chars, LogPhase, RequestTelemetry, TelemetrySnapshot};

pub const MAX_ATTEMPT_DETAILS: usize = 24;
pub const MAX_ATTEMPT_ERROR_CHARS: usize = 200;
pub const MAX_ATTEMPT_ACCOUNT_CHARS: usize = 60;
pub const MAX_ATTEMPT_RETRY_REASON_CHARS: usize = 120;
pub const MAX_ATTEMPT_NOTICE_CHARS: usize = 200;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AttemptDetail {
    pub provider: String,
    #[serde(default)]
    pub account: String,
    #[serde(rename = "accountId", default)]
    pub account_id: String,
    #[serde(rename = "candidateId", default)]
    pub candidate_id: String,
    #[serde(rename = "memberId", default)]
    pub member_id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub reasoning: String,
    #[serde(default)]
    pub reason: String,
    #[serde(rename = "stickyHit", default)]
    pub sticky_hit: bool,
    #[serde(rename = "startedAt", default)]
    pub started_at: Option<i64>,
    /// 响应头或会话式转发接收结果，不代表流式响应已经结束。
    #[serde(rename = "responseAt", default)]
    pub response_at: Option<i64>,
    #[serde(rename = "finishedAt", default)]
    pub finished_at: Option<i64>,
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub retries: Vec<RetryEvent>,
    #[serde(default)]
    pub notice: Option<String>,
    #[serde(rename = "bodyBytes", default)]
    pub body_bytes: Option<i64>,
    #[serde(rename = "errorPolicy", default, skip_serializing_if = "Option::is_none")]
    pub error_policy: Option<Value>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RetryEvent {
    pub reason: String,
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(rename = "delayMs", default)]
    pub delay_ms: u64,
}

impl TelemetrySnapshot {
    pub(super) fn current_attempt_mut(&mut self) -> Option<&mut AttemptDetail> {
        if self.attempts > MAX_ATTEMPT_DETAILS as i64 { return None; }
        self.attempts_detail.last_mut()
    }

    pub fn finish_trace(&mut self, finished_at: i64, error: Option<&str>) {
        let Some(last) = self.current_attempt_mut() else { return };
        if last.finished_at.is_none() {
            last.finished_at = Some(finished_at.max(last.started_at.unwrap_or(finished_at)));
        }
        if let Some(error) = error.filter(|text| !text.is_empty()) {
            if last.error.is_none() {
                last.error = Some(truncate_chars(error, MAX_ATTEMPT_ERROR_CHARS));
            }
        }
    }
}

impl RequestTelemetry {
    /// 写入这次请求的选路快照。
    ///
    /// 有些键**不属于选路**，但同住这份快照：图片兼容（`visionCompatibility`）
    /// 与特殊请求路由的改写记录（`compactRoute` / `titleRoute`）。它们在选路
    /// 之前就已确定，而选路会在每轮重试时整份覆盖 —— 不在这里搬过去，
    /// 回放页就只剩最后一次的选路结果，看不到「这条请求被改写过模型」。
    /// 整体替换路由快照时**要保留**的键。
    ///
    /// 路由快照会被多次整体替换（选路 → 错误策略 → 冷却…），每次替换都带上
    /// 这些「与本次请求相关但不由本次替换决定」的字段。`intent` 在列：意图分类
    /// 的结果是在选路**之前**算出来的，后面几次替换不能把它冲掉（实测踩过：
    /// 分类确实生效、候选确实收窄了，但快照里没有 intent，排查时完全看不出
    /// 分类发生过）。
    const PRESERVED: &'static [&'static str] = &["visionCompatibility", "compactRoute", "titleRoute", "errorPolicyConfig", "intent"];

    pub fn set_route_decision(&self, mut decision: Value) {
        let mut guard = self.lock();
        let carried: Vec<(&'static str, Value)> = Self::PRESERVED.iter()
            .filter_map(|key| guard.route.as_ref().and_then(|route| route.get(*key)).map(|value| (*key, value.clone())))
            .collect();
        if !carried.is_empty() {
            if let Some(object) = decision.as_object_mut() {
                for (key, value) in carried { object.insert(key.into(), value); }
            }
        }
        if guard.route.as_ref() == Some(&decision) { return; }
        guard.route = Some(decision);
        guard.route_revision = guard.route_revision.wrapping_add(1);
        self.flush_live(&guard);
    }

    pub fn route_decision(&self) -> Option<Value> {
        self.lock().route.clone()
    }

    pub fn note_error_policy(&self, policy: Value) {
        let mut guard = self.lock();
        if let Some(last) = guard.current_attempt_mut() {
            let mut next = last.error_policy.take().filter(Value::is_object).unwrap_or_else(|| serde_json::json!({}));
            if let (Some(object), Some(patch)) = (next.as_object_mut(), policy.as_object()) {
                for (key, value) in patch { object.insert(key.clone(), value.clone()); }
            }
            last.error_policy = Some(next);
        }
        self.flush_live(&guard);
    }

    pub fn note_attempt_response(&self) {
        let mut guard = self.lock();
        let now = crate::server::logging::now_ms();
        // 当前尝试的响应头时刻**总要**记（明细数组超限时 `current_attempt_mut()`
        // 会返回 None，那条时刻就丢了 —— 延迟采样会退化成「整段耗时」）。
        guard.current_response_at = Some(now);
        let Some(last) = guard.current_attempt_mut() else { return };
        if last.response_at.is_none() {
            last.response_at = Some(now);
            self.flush_live(&guard);
        }
    }

    /// 本次尝试的**首帧延迟**（毫秒），没有可用样本时 None。
    ///
    /// ── 为什么有两条口径 ────────────────────────────────────────
    /// 流式/无状态路径走 `send_with_retry`，收到响应头时会调
    /// [`RequestTelemetry::note_attempt_response`]，于是 `response_at` 有值 ——
    /// 那是真正的「首帧时刻」。
    ///
    /// 会话式与自定义提供商路径不走 `send_with_retry`，**从不**调那个钩子，
    /// `response_at` 恒为 None。若只认 `response_at`，这两条路径的延迟永远
    /// 采不到（延迟评分对它们完全失效）。所以这里回退到「当前时刻 −
    /// started_at」：那不是首帧，但对**非流式**响应而言，上游返回结果的时刻
    /// 本身就是「上游花了多久」的合理代理。
    ///
    /// 宁可要一个粗口径的样本，也不要整条路径没有样本。
    pub fn attempt_latency_ms(&self) -> Option<i64> {
        let guard = self.lock();
        // 当前尝试的开始时刻：与历史数组解耦（数组有 24 条上限，超限后
        // `last()` 会拿到陈旧尝试）。
        let started = guard.current_started_at?;
        // 响应头时刻（流式/无状态路径有）；没有就退到「现在」。
        let response_at = guard.current_response_at
            .or_else(|| guard.attempts_detail.last().and_then(|attempt| attempt.response_at));
        let end = response_at.unwrap_or_else(crate::server::logging::now_ms);
        Some(end.saturating_sub(started).max(0))
    }

    pub fn note_attempt_started(&self, provider: &str, account: &str) {
        let mut guard = self.lock();
        // 当前尝试的时刻**总要**记：明细数组有 24 条上限，超限后不再 push，
        // 但延迟采样要的是「当前这一轮」，不能退化成取更早的尝试。
        guard.current_started_at = Some(crate::server::logging::now_ms());
        // 上一轮的响应头时刻**必须清掉**：换号重试时，若新一轮因数组超限而
        // 没记到自己的响应头时刻，就会拿上一轮的旧值去减新一轮的开始时刻 ——
        // 算出一个跨轮的错误延迟（甚至负数）。
        guard.current_response_at = None;
        if guard.attempts_detail.len() >= MAX_ATTEMPT_DETAILS { return; }
        let account_id = guard.account_id.clone();
        let candidate_id = guard.route.as_ref().and_then(|route| route.get("pickedCandidate")).and_then(Value::as_str).unwrap_or(&account_id).to_string();
        let member_id = crate::server::core::routing_groups::member_key(&candidate_id).unwrap_or("").to_string();
        let reason = guard.route.as_ref()
            .and_then(|route| route.get("ordered")).and_then(Value::as_array)
            .and_then(|entries| entries.iter().find(|entry| entry.get("candidateId").and_then(Value::as_str).unwrap_or_else(|| entry.get("id").and_then(Value::as_str).unwrap_or("")) == candidate_id))
            .and_then(|entry| entry.get("reason")).and_then(Value::as_str).unwrap_or("");
        let reason = truncate_chars(reason, MAX_ATTEMPT_NOTICE_CHARS);
        let sticky_hit = guard.route.as_ref().and_then(|route| route.get("stickyHit")).and_then(Value::as_bool).unwrap_or(false);
        guard.attempts_detail.push(AttemptDetail {
            provider: provider.to_string(), account: truncate_chars(account, MAX_ATTEMPT_ACCOUNT_CHARS),
            account_id, candidate_id, member_id, model: String::new(), reasoning: String::new(), reason, sticky_hit,
            started_at: Some(crate::server::logging::now_ms()), response_at: None, finished_at: None,
            status: None, error: None, retries: Vec::new(), notice: None, body_bytes: None, error_policy: None,
        });
        self.enter_phase(&mut guard, LogPhase::Waiting);
        self.flush_live(&guard);
    }
}
