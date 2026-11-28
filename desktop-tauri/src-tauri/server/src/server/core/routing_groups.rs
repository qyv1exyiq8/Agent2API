//! 跨模型成员投影。运行时字段只存在于候选副本，不写入账号库。

use std::collections::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::server::config::{self, RoutingGroup};
use super::{account_store::AccountStore, capability, custom_providers, key_scope::{self, KeyScope}, models::model_id, providers, routing};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestNeeds {
    #[serde(default)] pub tools: bool,
    #[serde(default)] pub images: bool,
    #[serde(default)] pub video: bool,
}

impl RequestNeeds {
    pub fn from_body(body: &Value) -> Self {
        let mut needs = Self {
            tools: ["tools", "functions"].iter().any(|key| body.get(key).and_then(Value::as_array).is_some_and(|tools| !tools.is_empty())),
            ..Default::default()
        };
        for message in body.get("messages").and_then(Value::as_array).into_iter().flatten() {
            needs.tools |= matches!(message.get("role").and_then(Value::as_str), Some("tool" | "function"))
                || message.get("function_call").is_some()
                || message.get("tool_calls").and_then(Value::as_array).is_some_and(|calls| !calls.is_empty());
            for part in message.get("content").and_then(Value::as_array).into_iter().flatten() {
                match part.get("type").and_then(Value::as_str) {
                    Some("image_url" | "input_image" | "image") => needs.images = true,
                    Some("video_url" | "input_video" | "video") => needs.video = true,
                    _ => {},
                }
            }
        }
        needs
    }

    pub fn reason(&self, item: &Value) -> Option<String> {
        for (required, key, label) in [(self.tools, "supportsToolCall", "工具调用"),
            (self.images, "supportsImages", "图片输入"), (self.video, "supportsVideo", "视频输入")] {
            if required && item.get(key).and_then(Value::as_bool) != Some(true) {
                return Some(if item.get(key).and_then(Value::as_bool) == Some(false) {
                    format!("该模型不支持{label}，已跳过")
                } else { format!("该模型的{label}能力未声明，请先在模型管理中确认") });
            }
        }
        None
    }
}

fn manifest(provider: &str) -> Vec<Value> {
    if let Some(owner) = custom_providers::get(provider) {
        owner.get("models").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().map(|mut item| {
            super::model_meta::apply(&mut item);
            let overrides = capability::normalize_object(item.get("capabilities"));
            capability::apply_overrides(&mut item, &overrides);
            item
        }).collect()
    } else { providers::catalog::manifest_for_provider(provider) }
}

pub fn catalog_model(provider: &str, model: &str) -> Option<Value> {
    manifest(provider).into_iter().find(|item| model_id(item).eq_ignore_ascii_case(model))
}

pub fn choices(store: &AccountStore) -> Vec<Value> {
    let accounts = store.routing_snapshot();
    let mut ids: Vec<_> = accounts.accounts.iter().map(|account| routing::provider_of(account).to_string()).collect();
    ids.extend(providers::executable_provider_ids());
    ids.extend(custom_providers::list().iter().filter_map(|provider| provider.get("id").and_then(Value::as_str).map(str::to_string)));
    ids.sort(); ids.dedup();
    let mut result = Vec::new();
    for provider in ids {
        if !super::account_store::store_view::account_visible(&provider) { continue; }
        let models = manifest(&provider);
        let mut seen = HashSet::new();
        for item in models {
            let model = model_id(&item);
            if model.is_empty() || !seen.insert(model.to_ascii_lowercase()) { continue; }
            if providers::images::model_is_image(&item) { continue; }
            result.push(json!({ "provider": provider, "providerLabel": providers::label_of(&provider), "model": model,
                "name": item.get("name").and_then(Value::as_str).unwrap_or(&model), "capabilities": capability::effective(&item),
                "accountCount": accounts.accounts.iter().filter(|account| routing::provider_of(account) == provider).count() }));
        }
    }
    result
}

/// 从「每家有哪些模型」的原始清单里，归并出**跨厂同名**的模型。
///
/// 抽成纯函数（输入是 `choices` 的形状），这样归并规则能被单测覆盖 ——
/// 它是「同名组」的唯一判定逻辑，出错就会把不相干的模型并进一组。
///
/// 规则：
///   - 按模型名**小写**归并（大小写不同的同一个模型算同名）；
///   - 同一家的多个条目只算一家（同家别名不算「跨厂同名」）；
///   - 承载家 < 2 的不列（单家模型不是同名组）。
pub fn group_shared_models(choices: &[Value]) -> Vec<Value> {
    let mut by_model: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for row in choices {
        let Some(provider) = row.get("provider").and_then(Value::as_str) else { continue };
        let Some(model) = row.get("model").and_then(Value::as_str) else { continue };
        if model.is_empty() { continue; }
        by_model
            .entry(model.to_ascii_lowercase())
            .or_default()
            .push((provider.to_string(), model.to_string()));
    }
    let mut result: Vec<Value> = Vec::new();
    for (key, mut members) in by_model {
        members.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        // 同一家只留一条（同家别名不算「跨厂同名」）
        members.dedup_by(|left, right| left.0 == right.0);
        if members.len() < 2 { continue; }
        let providers: Vec<&str> = members.iter().map(|(provider, _)| provider.as_str()).collect();
        result.push(json!({
            // 对外名用第一家承载家的原样模型名（小写键只用于归并）
            "model": members[0].1,
            "key": key,
            "providers": providers,
            "members": members.iter().map(|(provider, model)| json!({
                "provider": provider,
                "model": model,
            })).collect::<Vec<_>>(),
        }));
    }
    // 承载家多的排前面 —— 那是最有值的容灾组
    result.sort_by(|left, right| {
        let left_count = left.get("providers").and_then(Value::as_array).map_or(0, Vec::len);
        let right_count = right.get("providers").and_then(Value::as_array).map_or(0, Vec::len);
        right_count.cmp(&left_count)
            .then(left.get("model").and_then(Value::as_str).unwrap_or("").cmp(right.get("model").and_then(Value::as_str).unwrap_or("")))
    });
    result
}

/// **跨厂同名模型**的发现结果：同一个模型名被几家提供商同时承载。
///
/// 这就是「自动同名组」（magpie 的 AutoStandIn）的原料：客户端只点一个
/// 模型名，转发层本来就已跨厂容灾（`route_for_forward` 返回全部承载该模型的
/// 厂），但用户**看不到**这件事，也没法手动把某几个名字绑成一个组。
/// 这个发现只做「可见 + 可一键成组」，不改变转发行为。
pub fn discover_shared_models(store: &AccountStore) -> Vec<Value> {
    let accounts = store.routing_snapshot();
    let mut shared = group_shared_models(&choices(store));
    // 补上各家的账号数与展示名（决定这个组值不值得建）
    for entry in shared.iter_mut() {
        let Some(members) = entry.get_mut("members").and_then(Value::as_array_mut) else { continue };
        for member in members.iter_mut() {
            let provider = member.get("provider").and_then(Value::as_str).unwrap_or("").to_string();
            let count = accounts.accounts.iter().filter(|account| routing::provider_of(account) == provider).count();
            member["accountCount"] = Value::from(count);
            member["providerLabel"] = Value::from(providers::label_of(&provider));
        }
    }
    shared
}

pub fn candidate_key(member: &str, account: &str) -> String { format!("{member}\0{account}") }pub fn account_key(key: &str) -> &str { key.split_once('\0').map(|(_, account)| account).unwrap_or(key) }
pub fn member_key(key: &str) -> Option<&str> { key.split_once('\0').map(|(member, _)| member) }
pub fn attempt_key(account: &Value) -> String {
    account.get("routingCandidateId").and_then(Value::as_str).unwrap_or_else(|| routing::account_id(account).unwrap_or("")).to_string()
}
pub fn candidate_model(account: &Value) -> Option<&str> { account.get("routingModel").and_then(Value::as_str) }
pub fn family_key(account: &Value) -> &str {
    account.get("routingMemberId").and_then(Value::as_str).unwrap_or_else(|| routing::provider_of(account))
}

#[derive(Clone, Default)]
pub struct GroupProjection {
    pub accounts: Vec<Value>,
    pub blocked: Vec<Value>,
}

fn provider_active(provider: &str) -> bool {
    super::account_store::store_view::account_visible(provider)
        && custom_providers::get(provider).map_or_else(|| providers::adapter::adapter_by_id(provider).is_some(), |owner| owner.get("enabled") != Some(&Value::Bool(false)))
}

pub fn project(group: &RoutingGroup, accounts: &[Value], needs: &RequestNeeds, scope: Option<&KeyScope>) -> GroupProjection {
    project_with_vision(group, accounts, needs, scope, None)
}

pub fn project_with_vision(group: &RoutingGroup, accounts: &[Value], needs: &RequestNeeds, scope: Option<&KeyScope>,
    vision: Option<&config::vision_compat::VisionSettings>) -> GroupProjection {
    let mut result = GroupProjection::default();
    let by_id: HashMap<_, _> = accounts.iter().filter_map(|account| routing::account_id(account).map(|id| (id, account))).collect();
    let manifests: HashMap<_, _> = group.members.iter().map(|member| member.provider.clone()).collect::<HashSet<_>>()
        .into_iter().map(|id| { let items = manifest(&id); (id, items) }).collect();
    for member in group.members.iter().filter(|member| member.included) {
        let item = manifests.get(&member.provider).and_then(|items| items.iter().find(|item| model_id(item).eq_ignore_ascii_case(&member.model))).cloned();
        let reason = if !key_scope::allows_provider(scope, &member.provider) { Some("网关 Key 不允许此提供商".to_string()) }
            else if !provider_active(&member.provider) { Some("提供商当前不可用".to_string()) }
            else if let Some(item) = &item {
                if providers::images::model_is_image(item) { Some("生图模型不能用于对话路由".to_string()) }
                else {
                    let mut requirements = needs.clone();
                    if vision.is_some_and(|settings| settings.mode_for(&member.provider, &member.model).is_some()) {
                        requirements.images = false;
                    }
                    requirements.reason(item)
                }
            } else { Some("模型已不在提供商目录中，配置保留但不参与转发".to_string()) };
        let actual = item.as_ref().map(model_id).unwrap_or_else(|| member.model.clone());
        if let Some(reason) = reason {
            result.blocked.push(json!({ "id": member.id, "memberId": member.id, "provider": member.provider,
                "providerLabel": providers::label_of(&member.provider), "model": actual, "name": actual, "state": "unavailable", "reason": reason }));
            continue;
        }
        if member.accounts.is_empty() {
            result.blocked.push(json!({ "id": member.id, "memberId": member.id, "provider": member.provider, "model": actual,
                "name": actual, "state": "excluded", "reason": "该成员没有参与账号" }));
        }
        for id in &member.accounts {
            let Some(account) = by_id.get(id.as_str()).filter(|account| routing::provider_of(account) == member.provider) else {
                result.blocked.push(json!({ "id": id, "memberId": member.id, "provider": member.provider, "model": actual,
                    "name": id, "state": "unavailable", "reason": "参与账号已移除或不属于此提供商" }));
                continue;
            };
            let mut account = (*account).clone();
            account["routingCandidateId"] = candidate_key(&member.id, id).into();
            account["routingMemberId"] = member.id.clone().into();
            account["routingModel"] = actual.clone().into();
            account["routingPriority"] = result.accounts.len().into();
            result.accounts.push(account);
        }
    }
    result
}

pub fn validate_members(group: &RoutingGroup, old: Option<&RoutingGroup>, accounts: &[Value]) -> Result<(), String> {
    for member in &group.members {
        let previous = old.and_then(|group| group.members.iter().find(|item| item.id == member.id && item.provider == member.provider && item.model == member.model));
        match catalog_model(&member.provider, &member.model) {
            Some(item) if providers::images::model_is_image(&item) => return Err("对话路由组不能加入生图模型".into()),
            None if previous.is_none() => return Err(format!("提供商中找不到模型 {}，请刷新后重试", member.model)),
            _ => {},
        }
        for id in &member.accounts {
            if let Some(account) = accounts.iter().find(|account| routing::account_id(account) == Some(id)) {
                if routing::provider_of(account) != member.provider { return Err("参与账号不属于所选提供商".into()); }
            } else if !previous.is_some_and(|item| item.accounts.contains(id)) { return Err("参与账号不存在，请刷新后重试".into()); }
        }
    }
    Ok(())
}

pub fn append_public(store: &AccountStore, scope: Option<&KeyScope>, data: &mut Vec<Value>) {
    let accounts = store.routing_snapshot();
    for group in config::routing_profiles().groups.values() {
        if !key_scope::allows_model(scope, &group.model) || data.iter().any(|item| model_id(item).eq_ignore_ascii_case(&group.model)) { continue; }
        let members: Vec<_> = group.members.iter().filter(|member| member.included && key_scope::allows_provider(scope, &member.provider)
            && provider_active(&member.provider)
            && accounts.available_providers.contains(&member.provider) && member.accounts.iter().any(|id|
                accounts.accounts.iter().any(|account| routing::account_id(account) == Some(id) && routing::provider_of(account) == member.provider && account.get("enabled") != Some(&Value::Bool(false)))))
            .filter_map(|member| catalog_model(&member.provider, &member.model)).filter(|item| !providers::images::model_is_image(item)).collect();
        if members.is_empty() { continue; }
        let mut item = json!({ "id": group.model, "name": group.name, "object": "model", "owned_by": "routing-group", "kind": "chat", "routing_group": true });
        for (key, public) in [("supportsToolCall", "supports_tool_call"), ("supportsImages", "supports_images"),
            ("supportsVideo", "supports_video"), ("supportsReasoning", "supports_reasoning")] {
            item[public] = members.iter().all(|item| item.get(key).and_then(Value::as_bool) == Some(true)).into();
        }
        for (key, public) in [("maxInputTokens", "max_input_tokens"), ("maxOutputTokens", "max_output_tokens")] {
            if members.iter().all(|item| item.get(key).and_then(Value::as_u64).is_some()) {
                item[public] = members.iter().filter_map(|item| item.get(key).and_then(Value::as_u64)).min().map(Value::from).unwrap_or(Value::Null);
            }
        }
        let mut modalities = vec!["text"];
        if item["supports_images"] == true { modalities.push("image"); }
        if item["supports_video"] == true { modalities.push("video"); }
        item["input_modalities"] = json!(modalities);
        item["created"] = 0.into();
        item["is_default"] = false.into();
        data.push(item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn choice(provider: &str, model: &str) -> Value {
        json!({ "provider": provider, "model": model })
    }

    /// 只有一家承载的模型不是「同名组」，不该被列出来。
    #[test]
    fn single_provider_models_are_not_shared() {
        let choices = vec![choice("a", "model-x"), choice("a", "model-y")];
        assert!(group_shared_models(&choices).is_empty());
    }

    /// 两家承载同名模型 → 归并成一个组，成员各带各家的上游名。
    #[test]
    fn same_model_across_providers_becomes_one_group() {
        let choices = vec![choice("a", "gpt-5"), choice("b", "gpt-5")];
        let groups = group_shared_models(&choices);
        assert_eq!(groups.len(), 1);
        let group = &groups[0];
        assert_eq!(group.get("model").and_then(Value::as_str), Some("gpt-5"));
        let providers: Vec<&str> = group.get("providers").and_then(Value::as_array)
            .expect("providers").iter().map(|v| v.as_str().unwrap_or("")).collect();
        assert_eq!(providers, vec!["a", "b"]);
        assert_eq!(group.get("members").and_then(Value::as_array).map(Vec::len), Some(2));
    }

    /// 大小写不同算同一个模型（上游命名常有大小写差异）。
    #[test]
    fn model_names_are_compared_case_insensitively() {
        let choices = vec![choice("a", "GPT-5"), choice("b", "gpt-5")];
        assert_eq!(group_shared_models(&choices).len(), 1);
    }

    /// 同一家多个条目（原生模型 + 别名）只算一家，不构成「跨厂同名」。
    #[test]
    fn same_provider_entries_count_once() {
        let choices = vec![choice("a", "gpt-5"), choice("a", "gpt-5-alias"), choice("b", "gpt-5")];
        let groups = group_shared_models(&choices);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].get("providers").and_then(Value::as_array).map(Vec::len), Some(2));
    }

    /// 承载家多的组排前面（最有容灾价值）。
    #[test]
    fn groups_with_more_providers_come_first() {
        let choices = vec![
            choice("a", "solo"), choice("b", "solo"),
            choice("a", "tri"), choice("b", "tri"), choice("c", "tri"),
        ];
        let groups = group_shared_models(&choices);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].get("model").and_then(Value::as_str), Some("tri"));
    }

    /// 集成测试：对**真实账号库**跑发现逻辑（临时库 + 两个不同厂商的账号）。
    ///
    /// 纯函数测试覆盖了归并规则，这里补一条端到端：账号进库 → 快照读出 →
    /// `discover_shared_models` 跑通。防止「纯函数对、接线错」。
    ///
    /// 不断言具体的模型名：模型清单来自**提供商目录**（各家插件/内置目录），
    /// 那是运行期数据，测试环境里未必有某款模型。断言的是**结构自洽**与
    /// 「每组至少两家」——那两条才是这个接口的契约。
    #[test]
    fn shared_model_discovery_runs_against_a_real_store() {
        let (db, _temp) = crate::server::db::test_temp::TempDb::open("shared-models");
        let store = crate::server::core::account_store::AccountStore::with_db(Some(db));
        // 账号 id 由 **uid** 派生：两条记录必须带不同的 uid（否则第二次写入
        // 变成「更新既有账号」，快照里就只剩一条）。uid 放在顶层还是 auth 里
        // 取决于各家；这里放 auth，与内置家的形状一致。
        for (provider, uid) in [("workbuddy", "uid-wb"), ("qoder", "uid-qd")] {
            store.add_account(
                &json!({ "auth": { "accessToken": format!("token-{uid}") }, "account": { "uid": uid } }),
                Some(provider),
                Some(provider),
            ).expect("账号应当能写入");
        }
        // 账号确实进了库（快照能读出来）
        let snapshot = store.routing_snapshot();
        let ids: Vec<String> = snapshot.accounts.iter()
            .map(|account| crate::server::core::routing::provider_of(account).to_string()).collect();
        assert!(snapshot.accounts.len() >= 2, "两个账号都应当进入选路快照，实际 {ids:?}");

        // 发现接口跑通且结构自洽
        for entry in discover_shared_models(&store) {
            assert!(!entry.get("model").and_then(Value::as_str).unwrap_or("").is_empty(),
                "每组都要有对外模型名");
            let providers = entry.get("providers").and_then(Value::as_array).expect("providers 必须是数组");
            assert!(providers.len() >= 2, "同名组至少两家：{providers:?}");
            let members = entry.get("members").and_then(Value::as_array).expect("members 必须是数组");
            assert_eq!(members.len(), providers.len(), "成员数与承载家数一致");
            for member in members {
                assert!(member.get("provider").and_then(Value::as_str).is_some(), "成员要有 provider");
                assert!(member.get("model").and_then(Value::as_str).is_some(), "成员要有上游模型名");
                assert!(member.get("accountCount").and_then(Value::as_u64).is_some(), "成员要有账号数");
            }
        }
    }
}
