//! 共享配额池的池级冷却（KV 存储）。
//!
//! ── 解决什么问题 ──────────────────────────────────────────────
//! 同一 `quotaPool` 下的账号共用一份上游额度。改造前每个账号撞 429 只冷自己，
//! 于是请求按优先级逐个去撞同一个已经空掉的池，直到整池账号全部被冷
//! （2026-10-13 真实事故：11 个同池账号一天内被逐个撞完后全部沉底）。
//! 这里让「其中一个撞额度」时整池一起短暂冷却，避免这种逐 account 的浪费。
//!
//! ── 为什么是短 TTL ────────────────────────────────────────────
//! 池级冷却标记的是「这个池的额度暂时没了」，不是「某个账号坏了」。额度通常
//! 按分钟级窗口恢复，所以用 [`POOL_COOLDOWN_MS`]（10 分钟）的短 TTL，到点自动
//! 放回；任何一次成功转发也会清掉它（`clear`），让恢复提前生效。绝不无限期。
//!
//! ── 存储形态 ──────────────────────────────────────────────────
//! 独立的 KV 条目 `routingPoolCooldowns`，形如
//! `{ poolId: { model: { "resetAt": i64 } } }`。
//! 与账号记录解耦：池是跨账号（甚至可跨 provider）的概念，挂在单条账号里会
//! 出现「写 A 账号却要改 B 账号记录」的尴尬；KV 是天然的多账号共享位置。
//! **按模型分桶**：与账号的 `rateLimits` 同口径 —— 一个模型撞额度不该冷掉
//! 整个池对所有模型的服务，否则是误伤。

use serde_json::{json, Value};

use crate::server::logging;

use super::store::AccountStore;

const POOL_STATE: &str = "routingPoolCooldowns";
/// 池级冷却时长（10 分钟，短 TTL）：与上游额度窗口同量级，到点自动恢复。
pub const POOL_COOLDOWN_MS: i64 = 10 * 60 * 1000;

/// 池级冷却截止时刻：取 `now + 短TTL` 与上游给的恢复时刻里更晚的那个。
///
/// 上游没给（或给了过去的时间）时用短 TTL 兜底 —— 绝不无限期冷却；
/// 上游给了更晚的时刻则尊重它（例如额度窗口明确 30 分钟后才重置）。
fn pool_reset_at(now: i64, reset_at: Option<i64>) -> i64 {
    let fallback = now.saturating_add(POOL_COOLDOWN_MS);
    reset_at.filter(|at| *at > now).unwrap_or(fallback).max(fallback)
}

/// 在状态对象里写入一条池冷却：`state[pool][model] = entry`，并顺手清掉
/// **该池**下已过期的模型记录（避免 KV 无限增长）。
///
/// 抽成纯函数：KV 的读改写逻辑可以不碰数据库就被完整测到（见本文件 tests）。
fn state_mark(state: &mut Value, pool: &str, model: &str, entry: &Value, now: i64) {
    if !state.is_object() {
        *state = json!({});
    }
    if !state[pool].is_object() {
        state[pool] = json!({});
    }
    if let Some(models) = state[pool].as_object_mut() {
        models.retain(|_, value| {
            value
                .get("resetAt")
                .and_then(Value::as_i64)
                .is_some_and(|at| at > now)
        });
        models.insert(model.to_string(), entry.clone());
    }
}

/// 读某池某模型当前仍有效的冷却（已过期 = None）。
fn state_get(state: &Value, pool: &str, model: &str, now: i64) -> Option<Value> {
    state
        .get(pool)
        .and_then(|models| models.get(model))
        .filter(|entry| {
            entry
                .get("resetAt")
                .and_then(Value::as_i64)
                .is_some_and(|at| at > now)
        })
        .cloned()
}

/// 删除某池某模型的冷却；返回是否真的删掉了什么（池里最后一个模型被删时
/// 连池键一起收掉，不留空对象）。
fn state_clear(state: &mut Value, pool: &str, model: &str) -> bool {
    let mut removed = false;
    if let Some(models) = state.get_mut(pool).and_then(Value::as_object_mut) {
        removed = models.remove(model).is_some();
        if models.is_empty() {
            state.as_object_mut().map(|object| object.remove(pool));
        }
    }
    removed
}

fn load_pools(conn: &rusqlite::Connection) -> rusqlite::Result<Value> {
    use rusqlite::OptionalExtension;
    let value: Option<String> = conn
        .query_row("SELECT value FROM kv WHERE key = ?1", [POOL_STATE], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(value
        .and_then(|value| serde_json::from_str(&value).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({})))
}

fn save_pools(conn: &rusqlite::Connection, value: &Value) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO kv(key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![POOL_STATE, value.to_string()])?;
    Ok(())
}

impl AccountStore {
    /// 让整个池的某个模型冷却到 `now + POOL_COOLDOWN_MS`（不早于上游给的 `reset_at`）。
    /// 返回冷却条目，供选路快照注入；pool id 或 model 为空时不做事。
    pub(crate) fn cool_pool(&self, pool: &str, model: &str, reset_at: Option<i64>) -> Option<Value> {
        let pool = pool.trim();
        if pool.is_empty() || model.is_empty() {
            return None;
        }
        let guard = self.guard();
        let now = logging::now_ms();
        let until = pool_reset_at(now, reset_at);
        let entry = json!({ "resetAt": until, "at": now });
        let saved = self
            .with_conn(&guard, |conn| {
                let mut state = load_pools(conn)?;
                state_mark(&mut state, pool, model, &entry, now);
                save_pools(conn, &state)
            })
            .is_ok();
        saved.then_some(entry)
    }

    /// 取某池某模型当前仍有效的冷却（已过期 = None）。选路快照逐账号调用。
    pub(crate) fn pool_cooldown_for(&self, pool: &str, model: &str) -> Option<Value> {
        let pool = pool.trim();
        if pool.is_empty() || model.is_empty() {
            return None;
        }
        let guard = self.guard();
        let now = logging::now_ms();
        self.with_conn(&guard, |conn| {
            let state = load_pools(conn)?;
            Ok(state_get(&state, pool, model, now))
        })
        .ok()
        .flatten()
    }

    /// 一次成功转发 → 清掉该池该模型的冷却（提前恢复）。
    pub(crate) fn clear_pool(&self, pool: &str, model: &str) {
        let pool = pool.trim();
        if pool.is_empty() || model.is_empty() {
            return;
        }
        let guard = self.guard();
        let _ = self.with_conn(&guard, |conn| {
            let mut state = load_pools(conn)?;
            if state_clear(&mut state, pool, model) {
                save_pools(conn, &state)?;
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn_with_kv() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE kv(key TEXT PRIMARY KEY, value TEXT NOT NULL)", []).unwrap();
        conn
    }

    #[test]
    fn pool_cooldown_constant_is_a_short_ttl() {
        // 必须是短 TTL：池级冷却标记的是额度暂时耗尽，不是账号坏了
        assert_eq!(POOL_COOLDOWN_MS, 10 * 60 * 1000);
    }

    #[test]
    fn pool_reset_at_uses_short_ttl_when_upstream_silent() {
        let now = 1_700_000_000_000i64;
        assert_eq!(pool_reset_at(now, None), now + POOL_COOLDOWN_MS);
    }

    #[test]
    fn pool_reset_at_respects_a_later_upstream_time() {
        let now = 1_700_000_000_000i64;
        let later = now + 30 * 60 * 1000;
        assert_eq!(pool_reset_at(now, Some(later)), later);
    }

    #[test]
    fn pool_reset_at_ignores_a_past_upstream_time() {
        let now = 1_700_000_000_000i64;
        // 上游给的是过去时刻（已过期的残留）→ 不采纳，用短 TTL 兜底
        assert_eq!(pool_reset_at(now, Some(now - 1000)), now + POOL_COOLDOWN_MS);
    }

    #[test]
    fn pool_reset_at_never_shorter_than_short_ttl() {
        let now = 1_700_000_000_000i64;
        // 上游给的时刻虽在未来但短于 TTL → 仍按 TTL，保证足够长的冷却
        let soon = now + 1000;
        assert_eq!(pool_reset_at(now, Some(soon)), now + POOL_COOLDOWN_MS);
    }

    /// 写入后能原样读回；不同模型互不影响（这是「不误伤同池其他模型」的核心）。
    #[test]
    fn mark_then_get_is_isolated_per_model() {
        let now = 1_700_000_000_000i64;
        let mut state = json!({});
        let entry = json!({ "resetAt": now + 60_000, "at": now });
        state_mark(&mut state, "pool-a", "model-x", &entry, now);

        assert_eq!(state_get(&state, "pool-a", "model-x", now), Some(entry));
        // 同池的另一个模型不受影响
        assert_eq!(state_get(&state, "pool-a", "model-y", now), None);
        // 别的池不受影响
        assert_eq!(state_get(&state, "pool-b", "model-x", now), None);
    }

    /// 过期即视为无冷却（不删也能读到 None；靠下次 mark 时顺带清理）。
    #[test]
    fn get_treats_expired_entry_as_absent() {
        let now = 1_700_000_000_000i64;
        let mut state = json!({});
        state_mark(&mut state, "pool-a", "model-x", &json!({ "resetAt": now - 1 }), now);
        assert_eq!(state_get(&state, "pool-a", "model-x", now), None);
    }

    /// 同一池内过期记录会在下一次 mark 时被清掉，KV 不会无限增长。
    #[test]
    fn mark_prunes_expired_siblings_in_the_same_pool() {
        let now = 1_700_000_000_000i64;
        let mut state = json!({});
        state_mark(&mut state, "pool-a", "expired", &json!({ "resetAt": now - 1 }), now);
        state_mark(&mut state, "pool-a", "fresh", &json!({ "resetAt": now + 60_000 }), now);

        let models = state["pool-a"].as_object().unwrap();
        assert!(!models.contains_key("expired"), "过期的同池记录应被清掉");
        assert!(models.contains_key("fresh"));
    }

    /// 清理只动指定模型；池里还有别的模型时池键保留，删空时池键一并收掉。
    #[test]
    fn clear_removes_only_the_target_model_and_drops_empty_pool() {
        let now = 1_700_000_000_000i64;
        let mut state = json!({});
        state_mark(&mut state, "pool-a", "model-x", &json!({ "resetAt": now + 60_000 }), now);
        state_mark(&mut state, "pool-a", "model-y", &json!({ "resetAt": now + 60_000 }), now);

        assert!(state_clear(&mut state, "pool-a", "model-x"));
        assert_eq!(state_get(&state, "pool-a", "model-x", now), None);
        assert!(state_get(&state, "pool-a", "model-y", now).is_some(), "同池其它模型必须保留");

        assert!(state_clear(&mut state, "pool-a", "model-y"));
        assert_eq!(state.get("pool-a"), None, "池内最后一个模型被清掉后，池键应一并移除");
        // 重复清理返回 false（不产生无意义的写盘）
        assert!(!state_clear(&mut state, "pool-a", "model-y"));
    }

    /// KV 落盘 → 读回的往返：真实走一遍 sqlite（不是纯内存对象）。
    #[test]
    fn kv_round_trip_through_sqlite() {
        let conn = conn_with_kv();
        let now = 1_700_000_000_000i64;

        // 初始为空对象（不是 null/字符串）
        assert_eq!(load_pools(&conn).unwrap(), json!({}));

        let mut state = load_pools(&conn).unwrap();
        state_mark(&mut state, "pool-a", "model-x", &json!({ "resetAt": now + 60_000 }), now);
        save_pools(&conn, &state).unwrap();

        let reloaded = load_pools(&conn).unwrap();
        assert_eq!(state_get(&reloaded, "pool-a", "model-x", now), Some(json!({ "resetAt": now + 60_000 })));

        // 覆盖同一键不产生重复
        let mut state = reloaded;
        state_mark(&mut state, "pool-a", "model-x", &json!({ "resetAt": now + 120_000 }), now);
        save_pools(&conn, &state).unwrap();
        let reloaded = load_pools(&conn).unwrap();
        assert_eq!(reloaded["pool-a"].as_object().unwrap().len(), 1);
        assert_eq!(state_get(&reloaded, "pool-a", "model-x", now).unwrap()["resetAt"], now + 120_000);

        // 清理后落盘，再读仍是空
        let mut state = reloaded;
        assert!(state_clear(&mut state, "pool-a", "model-x"));
        save_pools(&conn, &state).unwrap();
        assert_eq!(load_pools(&conn).unwrap(), json!({}));
    }

    /// 坏数据（非对象 JSON）读成空对象，不让池判定因脏数据崩掉。
    #[test]
    fn corrupt_kv_value_reads_as_empty() {
        let conn = conn_with_kv();
        conn.execute("INSERT INTO kv(key,value) VALUES (?1,?2)", rusqlite::params![POOL_STATE, "\"not-an-object\""]).unwrap();
        assert_eq!(load_pools(&conn).unwrap(), json!({}));
    }
}
