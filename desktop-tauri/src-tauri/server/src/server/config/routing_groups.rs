//! 自定义对话路由组。组是独立入口，成员直指提供商的模型，不借用模型映射。

use std::collections::{BTreeMap, HashSet};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use super::{RoutingProfile, RoutingSettings};

pub const MAX_GROUPS: usize = 500;
pub const MAX_MEMBERS: usize = 256;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutingMember {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub included: bool,
    pub accounts: Vec<String>,
}

/// 一个**意图档位**：分类器判到这一档时，请求应当走哪些成员。
///
/// 档位是「任务难易」这类语义标签，不是模型名 —— 分类器看到的是 `label`
/// 的描述，回一个编号；网关据此把候选收窄到 `members` 列出的成员。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IntentLevel {
    /// 档位标识（对应 `RoutingMember.id` 之一，或成员 id 列表的引用名）
    pub id: String,
    /// 给分类器看的档位描述（用一句话说明「什么样的请求属于这一档」）
    pub label: String,
    /// 命中这一档时保留的成员 id 列表（`RoutingMember.id`）。
    ///
    /// 空 = 不做收窄（这一档等价于「随便哪个成员都行」）。
    pub members: Vec<String>,
}

/// 路由组的**意图分类器**配置。
///
/// ── 它做什么 ────────────────────────────────────────────────
/// 用户回合开始时，用组内的某个成员（通常是小而快的那个）问一次「这条消息属于
/// 哪一档」，然后把候选收窄到那一档列出的成员。参照 magpie 的 AutoStandIn 思路，
/// 但落点不同：magpie 用 effort 调 `reasoning_effort`，本项目用**选成员**。
///
/// ── 分类器为什么是「组内成员」而不是「随便哪个模型」 ──────────────
/// 两个理由：
///   1. **magpie 就是这么做的** —— 「分类器的开销算在组上，所以它走组的账号，
///      和这一轮一样」（`internal/gateway/classify.go` 的 `held` 参数）。
///   2. **限制器是账号级的**。若分类器按模型名去全局队列里找承载家，它可能被
///      限流/限额规则拦下 —— 而那些账号用户根本没用在本组上。走组内成员则
///      与用户请求受同一套规则约束，语义一致。
///
/// ── 失败语义 ────────────────────────────────────────────────
/// 分类失败 / 超时 / 回答不可解析 → **不分类**，按原候选集合走。
/// 分类器是优化，不是依赖 —— 它坏了不能拖垮用户请求。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IntentClassifier {
    /// 分类器用**组内哪个成员**（`RoutingMember.id`）。
    pub classifier_member: String,
    /// 分类超时（毫秒）。超时就放弃分类，按原候选走。
    #[serde(default = "default_intent_timeout_ms")]
    pub timeout_ms: u64,
    /// 意图档位（至少一个；顺序即给分类器看的编号顺序）
    pub levels: Vec<IntentLevel>,
    /// 分类结果的缓存时长（毫秒）。0 = 不缓存。
    #[serde(default = "default_intent_cache_ms")]
    pub cache_ms: u64,
}

fn default_intent_timeout_ms() -> u64 { 8_000 }
fn default_intent_cache_ms() -> u64 { 600_000 }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutingGroup {
    pub id: String,
    pub name: String,
    pub model: String,
    #[serde(default)]
    pub description: String,
    pub strategy: String,
    pub sticky: String,
    pub members: Vec<RoutingMember>,
    /// 可选的意图分类配置（`None` = 不做意图分类，行为与从前逐字相同）
    #[serde(default)]
    pub intent: Option<IntentClassifier>,
}

impl RoutingGroup {
    pub fn settings(&self, mut base: RoutingSettings) -> RoutingSettings {
        base.strategy = super::RouteStrategy::from_id(&self.strategy);
        base.sticky = super::StickyMode::from_id(&self.sticky);
        base
    }

    pub fn profile(&self) -> RoutingProfile {
        let mut providers = Vec::new();
        for member in self.members.iter().filter(|member| member.included) {
            if !providers.contains(&member.provider) { providers.push(member.provider.clone()); }
        }
        RoutingProfile { provider_scope: Some(super::RoutingProviderScope::Selected), providers,
            strategy: Some(self.strategy.clone()), sticky: Some(self.sticky.clone()), ..Default::default() }
    }
}

pub fn revision(group: Option<&RoutingGroup>) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(&group).unwrap_or_default()))
}

pub fn validate(group: &RoutingGroup) -> Result<(), String> {
    fn identifier(value: &str) -> bool {
        !value.is_empty() && value.len() <= 128 && value.bytes().all(|ch| ch.is_ascii_alphanumeric() || b"-_.".contains(&ch))
    }
    if !identifier(&group.id) || group.members.iter().any(|member| !identifier(&member.id)) {
        return Err("路由组或成员标识无效".into());
    }
    if group.name.trim().is_empty() || group.name.chars().count() > 64 || group.name.chars().any(char::is_control)
        || group.description.chars().count() > 500 {
        return Err("组名不能为空或超过 64 个字符，说明不能超过 500 个字符".into());
    }
    if group.model.is_empty() || super::normalize_routing_model(&group.model)? != group.model {
        return Err("对外模型 ID 不能为空，请去掉首尾空格并使用小写英文字母".into());
    }
    super::validate_routing_profile(&group.profile())?;
    if group.members.len() > MAX_MEMBERS { return Err(format!("一个路由组最多允许 {MAX_MEMBERS} 个成员")); }
    let mut ids = HashSet::new();
    let mut pairs = HashSet::new();
    let mut count = 0;
    for member in &group.members {
        if !ids.insert(&member.id) || !pairs.insert((&member.provider, member.model.to_ascii_lowercase())) {
            return Err("同一提供商的同一模型不能重复加入，成员标识也不能重复".into());
        }
        if member.provider.is_empty() || member.provider.len() > 256 || member.provider.trim() != member.provider
            || member.provider.chars().any(char::is_control) || member.model.is_empty()
            || member.model.trim() != member.model || super::normalize_routing_model(&member.model).is_err() {
            return Err("成员的提供商或模型 ID 无效".into());
        }
        let unique: HashSet<_> = member.accounts.iter().collect();
        if unique.len() != member.accounts.len() || member.accounts.iter().any(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)) {
            return Err("成员的参与账号为空、重复或格式不正确".into());
        }
        count += member.accounts.len();
    }
    if count > super::routing_profiles::MAX_ROUTING_ACCOUNTS { return Err("路由组的账号候选数量过多".into()); }
    if let Some(intent) = &group.intent {
        validate_intent(intent, &group.members)?;
    }
    Ok(())
}

/// 意图分类配置的校验。
///
/// 两条硬要求：
///   1. 分类器模型必须**存在**（否则每次请求都要白等一次超时）；
///   2. 档位引用的成员必须**在本组内**（否则那一档永远选不出候选）。
fn validate_intent(intent: &IntentClassifier, members: &[RoutingMember]) -> Result<(), String> {
    // 分类器必须是**组内成员**（它的开销算在组上，且与用户请求受同一套限制器约束）
    if !members.iter().any(|member| member.id == intent.classifier_member) {
        return Err(format!("意图分类器成员 {} 不在本组内", intent.classifier_member));
    }
    if !members.iter().any(|member| member.id == intent.classifier_member && member.included) {
        return Err(format!("意图分类器成员 {} 未被启用（included=false）", intent.classifier_member));
    }
    if intent.levels.is_empty() {
        return Err("意图分类至少需要一个档位".into());
    }
    if intent.levels.len() > 32 {
        return Err("意图分类最多支持 32 个档位".into());
    }
    if intent.timeout_ms == 0 || intent.timeout_ms > 120_000 {
        return Err("意图分类超时必须在 1~120 秒之间".into());
    }
    let member_ids: HashSet<&str> = members.iter().map(|member| member.id.as_str()).collect();
    let mut level_ids: HashSet<&str> = HashSet::new();
    for level in &intent.levels {
        if !identifier_ok(&level.id) {
            return Err("意图档位标识无效".into());
        }
        if !level_ids.insert(level.id.as_str()) {
            return Err(format!("意图档位 {} 重复", level.id));
        }
        if level.label.trim().is_empty() || level.label.chars().count() > 200 {
            return Err(format!("意图档位 {} 的描述不能为空或超过 200 字", level.id));
        }
        for member_id in &level.members {
            if !member_ids.contains(member_id.as_str()) {
                return Err(format!("意图档位 {} 引用了不在本组的成员 {}", level.id, member_id));
            }
        }
    }
    // ── 刻意**不**在这里查模型目录 ─────────────────────────────
    // 分类器成员在不在目录里是**运行期状态**（插件有没有加载、模型有没有上下线），
    // 而这里是**持久配置**的准入校验。用运行期状态当准入条件会有一个坏后果：
    // 插件没加载时整组被判无效 → 组被静默丢弃 → 用户的组「不见了」，
    // 而他重启网关、插件加载后同一个配置又好了 —— 这种「配置时有时无」极难排查。
    //
    // 真正的处理在**运行期**：分类时若模型不可用，分类失败 → 不分类、按原候选走
    // （见 `core::intent_router::classify` 的失败语义）。配置永远保留，行为优雅降级。
    Ok(())
}

fn identifier_ok(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|ch| ch.is_ascii_alphanumeric() || b"-_.".contains(&ch))
}

pub(crate) fn from_value(value: Option<&Value>) -> BTreeMap<String, RoutingGroup> {
    value.and_then(Value::as_object).into_iter().flatten().take(MAX_GROUPS).filter_map(|(id, raw)| {
        let group: RoutingGroup = match serde_json::from_value(raw.clone()) {
            Ok(group) => group,
            Err(error) => {
                // 反序列化失败也要看得见 —— 否则组「静默消失」，用户只看到 404
                crate::server::logging::console_line("[Routing]", &format!("路由组 {id} 读取失败：{error}"));
                return None;
            }
        };
        if group.id != *id {
            return None;
        }
        match validate(&group) {
            Ok(()) => {
                // 组被接受了 —— 说一声（含有没有意图分类），便于排查「组在但没生效」
                crate::server::logging::console_line("[Routing]", &format!(
                    "路由组 {id} 已载入（对外模型名 {}，意图分类：{}）",
                    group.model,
                    if group.intent.is_some() { "有" } else { "无" }));
                Some((id.clone(), group))
            }
            Err(error) => {
                // 校验失败 = 组被丢弃（不进模型列表、点名得到 404）。
                // 必须出声：否则用户无从知道「我建的组为什么不见了」。
                crate::server::logging::console_line("[Routing]", &format!("路由组 {id} 校验未通过，已忽略：{error}"));
                None
            }
        }
    }).collect()
}

pub fn save(id: &str, group: Option<RoutingGroup>, expected: &str) -> Result<(), String> {
    if let Some(group) = &group {
        validate(group)?;
        if group.id != id { return Err("路由组标识不一致".into()); }
    }
    super::try_update_raw_field(super::KEY_ROUTING_PROFILES, |raw| {
        let mut raw_map = serde_json::Map::new();
        raw_map.insert(super::KEY_ROUTING_PROFILES.into(), raw.clone());
        let mut profiles = (*super::routing_profiles::from_raw(&raw_map)).clone();
        let old = profiles.groups.get(id);
        if revision(old) != expected { return Err("路由配置已在其他位置修改，请刷新后重试；草稿未丢失".to_string()); }
        if let Some(group) = &group {
            if old.is_none() && profiles.groups.len() >= MAX_GROUPS { return Err("自定义路由组数量已达上限".into()); }
            if profiles.groups.values().any(|known| known.id != id && known.model.eq_ignore_ascii_case(&group.model))
                || profiles.models.contains_key(&group.model)
                || !crate::server::core::providers::catalog::providers_for_model(&group.model).is_empty()
                || crate::server::core::model_rules::shared().mappings.iter().any(|mapping| mapping.alias.eq_ignore_ascii_case(&group.model))
                || crate::server::core::custom_providers::list().iter().any(|provider| provider.get("mappings").and_then(Value::as_array)
                    .is_some_and(|items| items.iter().any(|item| item.get("alias").and_then(Value::as_str).is_some_and(|alias| alias.eq_ignore_ascii_case(&group.model))))) {
                return Err("对外模型 ID 已被模型、映射或其他路由占用，请换一个名称".into());
            }
            for member in &group.members {
                if profiles.groups.values().any(|known| known.model.eq_ignore_ascii_case(&member.model)) || group.model.eq_ignore_ascii_case(&member.model) {
                    return Err("路由组不能包含自己或其他路由组".into());
                }
            }
            profiles.groups.insert(id.to_string(), group.clone());
        } else { profiles.groups.remove(id); }
        *raw = json!(profiles);
        Ok(())
    }, || "路由配置落盘失败，未应用本次修改".to_string())
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 实测用的组（trae 的小写难度阶梯）—— 复刻 `scripts/intent_lab.py` 写的形态。
    fn lab_group() -> RoutingGroup {
        let raw = json!({
            "id": "intent-lab",
            "name": "意图分类实测",
            "model": "intent-lab",
            "description": "意图分类路由实测",
            "strategy": "order",
            "sticky": "turn",
            "members": [
                { "id": "flash", "provider": "trae", "model": "deepseek-v4.1-flash",
                  "included": true, "accounts": ["trae-account-a", "trae-account-b"] },
                { "id": "pro", "provider": "trae", "model": "deepseek-v4.1-flash-max",
                  "included": true, "accounts": ["trae-account-c"] },
            ],
            "intent": {
                "classifierMember": "flash",
                "timeoutMs": 15000,
                "cacheMs": 600000,
                "levels": [
                    { "id": "easy", "label": "a greeting or a small question", "members": ["flash"] },
                    { "id": "hard", "label": "an architecture design or a subtle bug", "members": ["pro"] },
                ],
            },
        });
        serde_json::from_value(raw).expect("结构应能反序列化")
    }

    /// 组必须能通过 validate —— 否则它会被 `from_value` **静默丢弃**
    /// （组不进 /v1/models，点它的模型名得到 404，且没有任何日志说明为什么）。
    #[test]
    fn lab_group_passes_validation() {
        let group = lab_group();
        match validate(&group) {
            Ok(()) => {}
            Err(error) => panic!("实测用的组没通过校验：{error}"),
        }
    }

    /// 分类器成员**不在组内**时必须被拒（它压根选不出候选）。
    #[test]
    fn classifier_member_must_be_inside_the_group() {
        let mut group = lab_group();
        group.intent.as_mut().unwrap().classifier_member = "ghost".to_string();
        let error = validate(&group).expect_err("不在组内的分类器成员应当被拒");
        assert!(error.contains("不在本组内"), "实际错误：{error}");
    }

    /// 分类器成员**被停用**时必须被拒（它选不出候选）。
    #[test]
    fn classifier_member_must_be_included() {
        let mut group = lab_group();
        group.members[0].included = false;
        let error = validate(&group).expect_err("被停用的分类器成员应当被拒");
        assert!(error.contains("未被启用"), "实际错误：{error}");
    }

    /// 档位引用不存在的成员必须被拒（那一档永远选不出候选）。
    #[test]
    fn intent_level_must_reference_existing_members() {
        let mut group = lab_group();
        group.intent.as_mut().unwrap().levels[0].members = vec!["ghost".to_string()];
        let error = validate(&group).expect_err("引用不存在的成员应当被拒");
        assert!(error.contains("不在本组"), "实际错误：{error}");
    }
}
