"""意图分类路由 —— **真实环境实测**脚本。

用真实的数据库、真实的插件、真实的上游账号、真实的 HTTP 接口，验证：

  1. 分类器真的被调用了（而不是被跳过）
  2. 分类结果进了路由快照（`intent.levelId`）
  3. 候选按档位收窄了（简单请求只走 flash，复杂请求只走 pro）
  4. 分类失败时**不阻塞**用户请求（降级到原候选）

## 为什么每个场景都独立启动 server

server 在**启动时**把配置读进内存；之后改数据库不会自动生效（配置热加载
要走管理端点，自动化成本高）。所以「配置变了」的场景必须**重启 server**
才测得到真实行为 —— 否则测的是「改了库但 server 还拿着旧配置」的假象。

跑法（每个场景一次）：
    python scripts/intent_lab.py setup    [分类器模型]   # 写配置
    python scripts/intent_lab.py run      <port> [场景名]  # 发请求
    python scripts/intent_lab.py cleanup                  # 恢复
"""
import json
import os
import sqlite3
import sys
import time
import urllib.error
import urllib.request

DB = os.path.expanduser("~/.agent2api/agent2api.db")

GROUP_ID = "intent-lab"
GROUP_MODEL = "intent-lab"

# 实测用的临时网关 Key（明文写入 —— 解密逻辑对非 enc1: 开头的内容直接当明文用）。
# 用完即删，不影响用户已有的 Key。
TEST_KEY = "intent-lab-tmp-key-2026"
TEST_KEY_ID = "intent-lab-test"

# 用 **zcode**：实测时 trae/workbuddy 余额为 0，loomy 对话 60 秒无响应，
# qoder 会「模型正在排队」且被前面的失败留下持久冷却 —— 都是账号/上游状态，
# 不是本功能的毛病。zcode 余额 800 万、三个账号、五个模型实测 1.4~3.0 秒
# 稳定出内容。
# 用 **zcode**：实测时 trae/workbuddy 余额为 0，loomy 对话 60 秒无响应，
# qoder 会「模型正在排队」且被前面的失败留下持久冷却。zcode 余额 800 万、
# 三个账号、五个模型实测 1.4~3.0 秒稳定出内容。
ZCODE_ACCOUNTS = [
    "zcode-account-ebb5d6a19031b2d0979330fc832010e0f807e63a72a8844406fb1405b3441968",
    "zcode-account-b80b120cba001a0cb901da46edec29bedeb679879a23f4f6c21d5f75b306ec1b",
    "zcode-account-4d88a19d452376ae0379f4fc49cfcc4fe4f47dab0fe021bb75efeff4aea1836e",
]

# 难度阶梯：真实存在的两个 trae 模型。
#
# ⚠️ 模型名必须**全小写**：`config::routing_groups::validate` 要求
# `normalize_routing_model(model) == model`，而那个函数返回小写版 ——
# 大写名的成员会被判为无效，整组被静默丢弃（组不进入 /v1/models，
# 点它的模型名得到 404）。
# zcode 的真实难度阶梯（五个模型实测都能稳定出内容）
FLASH_MODEL = "glm-5.3-flash"
PRO_MODEL = "glm-5.3"
CLASSIFIER_MEMBER = "flash"


def log(*args):
    print(*args, flush=True)


def db_connect():
    return sqlite3.connect(DB)


def read_json(conn, key, default):
    row = conn.execute("SELECT value FROM kv WHERE key=?", (key,)).fetchone()
    if not row or not row[0]:
        return default
    try:
        return json.loads(row[0])
    except json.JSONDecodeError:
        return default


def write_json(conn, key, value):
    conn.execute("UPDATE kv SET value=? WHERE key=?", (json.dumps(value, ensure_ascii=False), key))
    conn.commit()


def build_group(classifier_member=CLASSIFIER_MEMBER):
    """构造测试路由组（真实配置形态，与用户在 UI 里建的一模一样）。"""
    return {
        "id": GROUP_ID,
        "name": "意图分类实测",
        "model": GROUP_MODEL,
        "description": "意图分类路由实测",
        "strategy": "order",
        "sticky": "turn",
        "members": [
            {
                "id": "flash",
                "provider": "zcode",
                "model": FLASH_MODEL,
                "included": True,
                "accounts": ZCODE_ACCOUNTS[:2],
            },
            {
                "id": "pro",
                "provider": "zcode",
                "model": PRO_MODEL,
                "included": True,
                "accounts": ZCODE_ACCOUNTS[2:],
            },
        ],
        # ── 意图分类配置 ──
        "intent": {
            # 分类器是**组内的一个成员**（magpie 的做法：分类器的开销算在组上，
            # 走组内账号，与用户请求受同一套限制器约束）。
            "classifierMember": classifier_member,
            "timeoutMs": 15000,
            "cacheMs": 600000,
            "levels": [
                {
                    "id": "easy",
                    # 描述用**英文**：分类器模型对英文档位描述的判别更稳
                    # （magpie 的提示词也是英文的）
                    "label": "a greeting, a small question, or a one-line mechanical change; little thought needed",
                    "members": ["flash"],
                },
                {
                    "id": "hard",
                    "label": "an architecture or algorithm design, a subtle concurrency or performance bug, or a long multi-step plan; careful deep thought needed",
                    "members": ["pro"],
                },
            ],
        },
    }


def add_temp_key(conn):
    entries = read_json(conn, "apiKeys", [])
    entries = [entry for entry in entries if entry.get("id") != TEST_KEY_ID]
    entries.append({
        "id": TEST_KEY_ID,
        "name": "意图分类实测",
        "key": TEST_KEY,
        "enabled": True,
        "allowedProviders": [],
        "allowedModels": [],
        "createdAt": 0,
    })
    write_json(conn, "apiKeys", entries)


def setup(classifier_member=CLASSIFIER_MEMBER):
    """**第一阶段**：写配置。必须在 server **启动之前**跑。"""
    conn = db_connect()
    add_temp_key(conn)
    profiles = read_json(conn, "routingProfiles", {"global": {}, "groups": {}, "models": {}})
    profiles.setdefault("groups", {})[GROUP_ID] = build_group(classifier_member)
    write_json(conn, "routingProfiles", profiles)
    log(f"[setup] 路由组 {GROUP_ID} 已写入（对外模型名 {GROUP_MODEL}）")
    log(f"[setup]   简单档 easy → flash({FLASH_MODEL})")
    log(f"[setup]   困难档 hard → pro({PRO_MODEL})")
    log(f"[setup]   分类器 → 组内成员 {classifier_member}")
    conn.close()


def cleanup():
    """**第三阶段**：恢复配置与 Key 列表。"""
    conn = db_connect()
    profiles = read_json(conn, "routingProfiles", {"global": {}, "groups": {}, "models": {}})
    profiles.setdefault("groups", {}).pop(GROUP_ID, None)
    write_json(conn, "routingProfiles", profiles)
    entries = read_json(conn, "apiKeys", [])
    entries = [entry for entry in entries if entry.get("id") != TEST_KEY_ID]
    write_json(conn, "apiKeys", entries)
    conn.close()
    log("[cleanup] 已恢复原配置与 Key 列表")


def http_json(base, method, path, payload=None, timeout=120):
    url = f"{base}{path}"
    data = json.dumps(payload).encode() if payload is not None else None
    req = urllib.request.Request(url, data=data, method=method,
                                 headers={"Content-Type": "application/json",
                                          "x-api-key": TEST_KEY})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            raw = response.read().decode("utf-8", "replace")
            try:
                return response.status, json.loads(raw)
            except json.JSONDecodeError:
                return response.status, raw
    except urllib.error.HTTPError as error:
        raw = error.read().decode("utf-8", "replace")
        try:
            return error.code, json.loads(raw)
        except json.JSONDecodeError:
            return error.code, raw
    except Exception as error:  # noqa: BLE001
        return 0, str(error)


def chat(base, message, model=GROUP_MODEL):
    status, body = http_json(base, "POST", "/v1/chat/completions", {
        "model": model,
        "messages": [{"role": "user", "content": message}],
        "stream": False,
        "max_tokens": 32,
    })
    return status, body


def clear_cooldown(base, provider, model, account_id=""):
    """用**模型测试**端点清掉该账号的冷却。

    为什么走这个端点：冷却存在账号数据里（加密的 `rateLimits` 字段），
    没有别的清除入口；而模型测试成功后会 `clear_policy_cooldown` +
    `clear_rate_limit`（见 `provider_loop` 的成功收尾）—— 这是真实可用的
    「这个账号刚才还好好的」证据，顺带把冷却清了。

    实测时上游偶发排队/限流会留下持久冷却，不清掉后续场景全被挡在门外。
    冷却是**每账号**一份，所以要按账号逐个清。
    """
    payload = {"provider": provider, "model": model}
    if account_id:
        payload["account_id"] = account_id
    status, body = http_json(base, "POST", "/api/models/test", payload, timeout=90)
    detail = ""
    if isinstance(body, dict):
        if body.get("error"):
            detail = str(body["error"])[:70]
        else:
            # 成功也要看清结果体：模型测试「成功」才清冷却；失败则冷却还在
            detail = json.dumps({k: body.get(k) for k in ("success", "status", "error", "reply") if k in body},
                                ensure_ascii=False)[:120]
    tag = account_id[-12:] if account_id else "(自动)"
    log(f"  [清冷却] {provider}/{model} acct={tag} → HTTP {status} {detail}")
    return status == 200


def latest_route_snapshot():
    conn = db_connect()
    try:
        row = conn.execute("SELECT route FROM requests ORDER BY row_id DESC LIMIT 1").fetchone()
        if not row or not row[0]:
            return None
        try:
            return json.loads(row[0])
        except (json.JSONDecodeError, TypeError):
            return None
    finally:
        conn.close()


def describe(snapshot):
    """把路由快照里与意图分类相关的部分讲清楚。"""
    if not isinstance(snapshot, dict):
        return "（无快照）"
    intent = snapshot.get("intent")
    picked = snapshot.get("pickedCandidate")
    ordered = snapshot.get("ordered") or []
    rows = []
    for entry in ordered[:6]:
        member = entry.get("memberId")
        model = entry.get("model") or entry.get("routingModel")
        rows.append(f"{member or entry.get('id')}@{model}")
    return (
        f"intent={json.dumps(intent, ensure_ascii=False)} | "
        f"picked={picked} | 候选={rows}"
    )


def scenario(base, name, message):
    log(f"\n--- {name} ---")
    status, body = chat(base, message)
    log(f"  HTTP {status}")
    text = ""
    if isinstance(body, dict):
        choices = body.get("choices") or []
        if choices:
            text = str((choices[0].get("message") or {}).get("content", ""))[:150]
        elif body.get("error"):
            text = f"ERROR: {body['error']}"
    else:
        text = str(body)[:150]
    log(f"  回答: {text}")
    if isinstance(body, dict) and not text:
        log(f"  [原始响应] {json.dumps(body, ensure_ascii=False)[:400]}")
    time.sleep(0.5)
    snapshot = latest_route_snapshot()
    log(f"  {describe(snapshot)}")
    return status, snapshot


def main(base, scenario_name):
    # 先把上一轮留下的持久冷却清掉（冷却存在账号数据里，不清会挡在门外）
    log("[prep] 清理账号冷却（逐账号 × 逐模型）")
    for account in ZCODE_ACCOUNTS:
        for account_model in (FLASH_MODEL, PRO_MODEL):
            clear_cooldown(base, "zcode", account_model, account)
    if scenario_name == "degraded":
        # 分类器成员不存在 → 分类失败 → 必须降级（不分类，按原候选走），
        # 且**用户请求必须成功**（分类器坏了不能拖垮请求）
        status, snapshot = scenario(base, "降级：分类器成员不存在", "你好")
        log("  期望：HTTP 200（分类失败不拖垮请求）；候选应是**全部**成员（未收窄）")
        return
    if scenario_name == "easy":
        scenario(base, "简单请求（你好）→ 应判 easy → flash", "你好")
        return
    if scenario_name == "hard":
        scenario(base, "复杂请求（架构设计）→ 应判 hard → pro",
                 "设计一个支持每秒 50 万写入的分布式任务调度系统，需要处理幂等、"
                 "跨机房故障转移、以及任务重试的指数退避。请给出架构方案与关键代码。")
        return
    # 默认：两个场景都跑
    scenario(base, "简单请求（你好）→ 应判 easy → flash", "你好")
    scenario(base, "复杂请求（架构设计）→ 应判 hard → pro",
             "设计一个支持每秒 50 万写入的分布式任务调度系统，需要处理幂等、"
             "跨机房故障转移、以及任务重试的指数退避。请给出架构方案与关键代码。")


if __name__ == "__main__":
    phase = sys.argv[1] if len(sys.argv) > 1 else "run"
    if phase == "setup":
        setup(sys.argv[2] if len(sys.argv) > 2 else CLASSIFIER_MEMBER)
    elif phase == "setup-broken":
        # 降级场景：给分类器成员分配一个**不存在的账号** → 组本身有效（能通过
        # 校验、能进模型列表），但分类器在运行时**没有可用候选** → 分类必然失败。
        #
        # 注意不能用「分类器成员指向不存在的成员」：那种配置会被 validate 直接拒，
        # 整组被丢弃 → 变成 404，测的是另一回事。
        conn = db_connect()
        add_temp_key(conn)
        profiles = read_json(conn, "routingProfiles", {"global": {}, "groups": {}, "models": {}})
        group = build_group()
        for member in group["members"]:
            if member["id"] == CLASSIFIER_MEMBER:
                member["accounts"] = ["no-such-account-id"]
        profiles.setdefault("groups", {})[GROUP_ID] = group
        write_json(conn, "routingProfiles", profiles)
        conn.close()
        log("[setup] 分类器成员被分配了不存在的账号（降级场景）")
    elif phase == "key":
        # 只加临时 Key（连通性排查用，不写路由组）
        conn = db_connect()
        add_temp_key(conn)
        conn.close()
        log("[setup] 临时 Key 已加入")
    elif phase == "cleanup":
        cleanup()
    else:
        port = sys.argv[2] if len(sys.argv) > 2 else "3077"
        main(f"http://127.0.0.1:{port}", sys.argv[3] if len(sys.argv) > 3 else "all")
