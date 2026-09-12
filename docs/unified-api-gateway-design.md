# 统一 API 网关与资源调度 · 设计方案

> 版本：v1.2（2026-09-11，v1.1 基础上：池标识 wb → buddy；补 Trae 历史数据兼容清单，Buddy 侧不保历史）
> 状态：已实施（Phase 1~3 全部落地，2026-09-12 通过九大类黑盒审查并完成修复轮）
> 关联：work-credit-pool-design.md（积分池）、workbuddy-product-design.md（Buddy 产品设计）

---

## 1. 背景与目标

### 1.1 现状问题

当前网关与资源管理分散在两个应用页面中，职责纠缠：

| 问题 | 现状 |
|---|---|
| 网关归属模糊 | API 服务（网关/Key/生态）内嵌在 Trae 页，Buddy 页又复制了一份网关状态/上游开关 |
| 模型目录割裂 | Trae 模型列表（api_models.json）与 Buddy 目录（wb_model_catalog.json）各自维护、各自展示 |
| 分流规则隐晦 | 按模型名命中 WB 目录即走 WB（WB 优先），Trae 默认模型 glm-5.2/5.3 实际被吸进 WB 消耗 WB 积分，用户无感知、不可配置 |
| 生态接入分叉 | CC Switch 注册分 Trae/WB 双条目（side 参数），但两者打到同一网关，区分仅是入口命名 |
| 管理入口深 | 想看网关状态/加个 Key 必须切到对应应用再进 API 服务页 |

### 1.2 设计目标

1. **公共化**：API 网关（接口、API Key、生态接入、用量统计）抽离为全局公共模块，与具体应用解耦；
2. **统一模型目录**：归纳 Trae + Buddy 全部支持模型，按模型 ID 去重，对外提供单一列表（模型 ID、展示名、积分倍率、思考档位、上下文、图片支持）；
3. **资源化调度**：Trae 和 Buddy 降级为两份"服务资源"，请求按模型 ID 匹配可用资源池，按可配置的调度策略统一调度；
4. **生态统一**：CC Switch 等生态注册不再区分 Trae/Buddy，只注册统一网关条目；
5. **应用页面聚焦**：原两个"API 服务"页改造为各自应用的"资源调度"页，只管本应用的资源池与模型目录。

---

## 2. 总体架构

```
┌─────────────────────────────────────────────────────────────┐
│  全局层（与 activeApp 无关）                                   │
│                                                              │
│  ┌─────────────── API 管理弹窗（Sidebar 左下角入口）─────────┐ │
│  │ 启停服务 · 指标行 · 接口配置 · API Keys · 生态接入       │ │
│  │ 目前资源 · 用量统计（含模型分布）                          │ │
│  └──────────────────────────────────────────────────────────┘ │
│                                                              │
│  ┌──────────────── 统一网关（127.0.0.1:7864，单实例）─────────┐ │
│  │ 鉴权（共享 api_keys.json）                                 │ │
│  │   → 模型名归一化（四段路由管线，保留）                       │ │
│  │   → 统一模型目录查表（sources: 支持该模型的资源池）          │ │
│  │   → 资源调度策略选池（dispatch_policy.json）               │ │
│  │   → 池内账号调度（沿用各池现有策略）                         │ │
│  │   → 记账（days / wb_days 分桶保留，按资源维度）            │ │
│  └──────────────┬───────────────────────────┬───────────────┘ │
│                 ▼                           ▼                 │
│  ┌──────── Trae 资源池 ──────┐   ┌─────── Buddy 资源池（wb_*）──┐ │
│  │ vault JWT 账号             │   │ WB 带凭证账号（wb_pool）    │ │
│  │ expire_first / 分组 / 白名单 │   │ 粘性会话 / 模型级冷却       │ │
│  │ 消耗：Trae 通用积分         │   │ 消耗：Buddy 积分            │ │
│  └────────────────────────────┘   └────────────────────────────┘ │
│                                                              │
│  ┌──── Trae · 资源调度页 ────┐    ┌──── Buddy · 资源调度页 ───┐ │
│  │ 积分体系 · 池指标          │    │ 积分体系 · 池指标          │ │
│  │ 账号池选择 · Trae 模型目录  │    │ 资源开关 · 账号池选择       │ │
│  └───────────────────────────┘    │ Buddy 模型目录（wb_catalog）│ │
│                                   └───────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

核心转变：**从"按应用内嵌 API 页"变为"全局网关 + 应用资源页"**。网关归公共，资源归应用。

> **命名约定（v1.2）**：对外/调度层的资源池标识统一为 **`trae` / `buddy`**——统一目录 `sources[].pool`、`dispatch_policy` 取值、请求日志 `pool=` 字段、回退日志池名均用 `buddy`。存量内部实现的 `wb_*` 前缀（`wb_model_catalog.json`、`wb_enabled`、`wb_days`、`wb_sticky_sessions.json` 等）**保留不改名**：纯重命名无收益且引入数据迁移风险，实施时在统一目录聚合层做 `wb → buddy` 的标识映射。

---

## 3. 公共模块：统一模型目录

### 3.1 数据模型

```jsonc
// 派生视图（不新增落盘文件，实时聚合两个源）
// 源1: data/api_models.json      （Trae 官网同步 + 内置补位）
// 源2: data/wb_model_catalog.json（Buddy 目录，完整元数据）

// 统一目录条目（api_unified_models 命令返回）
{
  "id": "glm-5.3",
  "display": "GLM-5.3",
  "rate": 0.78,                    // 实际生效倍率 = 当前调度策略命中的来源侧（见 4.2），非"最优值"
  "efforts": ["low", "medium", "high"],   // 思考档位（语义差异见注）
  "context_length": 1000000,       // 上下文（Max 模式 1M）
  "max_tokens": 96000,
  "supports_image": true,          // 图片
  "sources": [                     // 该模型可用的资源池（运行时附 enabled 派生标记）
    { "pool": "trae",  "rate": 0.78, "enabled": true },   // 官网接口返回倍率
    { "pool": "buddy", "rate": 0.79, "enabled": true }     // 内部实现为 wb_*
  ]
}
```

> **efforts 语义注**：Buddy 侧 `supported_efforts / effort_override`（wb_model_catalog）是透传上游的 `reasoning_effort` 参数；Trae 侧是模型自带的 Thinking 行为开关。两者合并展示，但仅在走 Buddy 池时作为请求参数下发。

### 3.2 Trae 模型元数据：四层来源，逐级兜底

Trae 官网 `batch_get_detail_param` 接口实际返回倍率等运营字段（客户端模型下拉即同源数据，含 `0.06x–1.83x` 倍率与限时折扣/会员折扣标签），当前 `ModelOption` 仅解析 `id/label`，其余丢弃。改造为扩展解析 + 四层兜底：

| 层级 | 来源 | 说明 | 优先级 |
|---|---|---|---|
| L1 人工维护 | `data/trae_model_meta.json` 覆盖层 | 模型管理页面编辑落盘（§6.1），同步永不覆盖 | 最高 |
| L2 官网同步 | `batch_get_detail_param` 响应扩展解析 | 倍率等字段随「同步官网模型」更新写入 `api_models.json` 条目（字段名以实际响应为准，Phase 1 抓包确认） | 高 |
| L3 文档参考值 | 官方文档内置表 | Max 模式上下文（见下）、内置模型清单 | 中 |
| L4 名称推断 | 代码内置规则 | 按模型名系列推断，无法确定显示 `—` | 兜底 |

**L3 文档参考值**（随版本更新维护）：

- 上下文（Max 模式）：Seed-Evolving / GLM-5.3 / GLM-5.2 / DeepSeek-V4-Pro（含正式版）/ DeepSeek-V4-Flash（含正式版）/ Kimi-K3 / MiniMax-M3 / Qwen3.8-Max / Qwen3.7-Plus → 1M（参考 [Max 模式文档](https://docs.trae.cn/ide_max-mode)）；其余默认 128K；
- 内置模型清单与自定义模型高级配置（上下文窗口 / 思考模式 / 支持图片输入等参数语义）参考 [内置模型文档](https://docs.trae.cn/ide_models)、[企业版模型设置](https://docs.trae.cn/enterprise_model-settings-for-trae-enterprise)；
- 倍率初始参考（客户端下拉实测，随同步覆盖）：Seed-Evolving 0.08 / Seed-2.1-Pro 0.08 / Seed-2.1-Turbo 0.20 / Seed-Code 0.06 / GLM-5.3-Flash 0.06 / GLM-5.3 0.78 / GLM-5.2 0.78 / DeepSeek-V4-Flash 0.16 / DeepSeek-V4-Pro 0.72 / Kimi-K3 1.83 / Kimi-K2.7-Code 0.83。

**L4 名称推断规则**（可被 L1–L3 覆盖）：

- 思考档位：DeepSeek-4 / Kimi-K2·K3 / GLM-5 系列默认开 Thinking（企业版模型系列文档），推断 `["medium", "high"]`；Flash/Turbo 轻量系列推断 `["low", "medium"]`；
- 图片支持：名称含 `V`（如 GLM-5V-Turbo）/ Seed 系列（豆包多模态）推断支持；Code / Flash 系列推断不支持；
- 上下文：`max` / `pro` / `evolving` 后缀且在 Max 模式表中 → 1M；`flash` / `turbo` → 128K。

### 3.3 去重合并规则

1. **归并键**：`canonical_id()` = trim + 小写后的模型 ID，与 `wb_catalog::find` 语义一致；**三处键统一走同一函数**：目录归并键、`dispatch_policy.per_model` 键、`trae_model_meta.json` 覆盖层键，防止规则漂移；
2. **双源命中**：`sources` 记录两侧；顶层 `rate` = 策略命中侧倍率（§4.2），`display / supports_image` 亦按**策略命中侧**选定（v1.2 修订：不再无条件取 WB 值；命中侧未声明时有值兜底），`display` 在 L1 人工 label 存在时绝对优先（§3.2）；`efforts / context / max_tokens` 取**有值优先**（WB 目录结构化元数据为主源——efforts 仅 Buddy 池作为请求参数下发，见 §3.1 注）；
3. **单源命中**：仅记录该池，另一池不出现；
4. **目录序**：默认按"双源在前、单源在后，同组内字母序"，资源页展示时按所属池单独过滤；
5. **可用性标记**：`sources[].enabled` 为运行时派生（wb_enabled / 账号池健康状态），不落盘。

### 3.4 对外暴露与维护

- **命令**：`api_unified_models(available_only?)` → 聚合视图（`available_only=true` 时过滤掉全部来源不可用的模型），供 API 管理弹窗与诊断用；
- **HTTP**：`GET /v1/models` 改为返回统一目录（去重合并），条目附 `owned_by: "unified"` 与 `sources`，客户端一处可见全部可用模型；**wb_enabled=false 时**：仅 Buddy 源的模型从返回中过滤，双源模型保留（仍可由 Trae 源服务）；
- **维护**：两个源文件仍各自独立维护（Trae 官网同步按钮、WB 目录同步/自定义），聚合是纯派生，不引入第三份持久化；
- **人工维护**：Trae 模型元数据通过资源调度页「模型目录」的编辑功能落盘 `trae_model_meta.json`（L1 覆盖层，键为 canonical_id），字段：`rate / efforts / context_length / max_tokens / supports_image`，编辑后即时生效（聚合视图实时计算）；
- **`ModelOption` 扩展**（向后兼容，`serde default`）：`api_models.json` 条目由 `{id, label}` 扩展为 `{id, label, rate?, context_length?, efforts?, supports_image?}`，旧文件可直接读取。

---

## 4. 统一调度器

### 4.1 请求处理流程（改造 `routes.rs` 分流点）

```
请求（任意 Key / 任意客户端）
 ① 鉴权：共享 api_keys（含 uid 白名单/专一绑定，跨池语义保留）
 ② 模型名归一化：wb_model_route 四段管线（别名→通配→内置系列→-thinking 后缀）
    —— 定位为"名字归一化层"，全保留，不受本次重构影响
 ③ 查统一目录：canonical_id 的 sources → 可用资源池集合（剔除 enabled=false 的源）
 ④ 选池：会话池粘性命中（§4.4，TTL 内）→ 否则按 dispatch_policy 优先级序
    （全局 + per_model 覆盖，键为 canonical_id）
 ⑤ 池内调度：
    · Trae 池：expire_first / 分组过滤 / Key 白名单
    · WB 池：粘性会话 → 专一 → 白名单过滤轮换，模型级冷却
 ⑥ 池间故障转移（仅双源模型）：首选池不可用 → 回退下一池（§4.3 / §4.4）
    单源模型资源不可用 → 显式报错（不静默换池）
 ⑦ 记账：is_wb 分桶（days / wb_days），不变
```

### 4.2 调度策略配置

```jsonc
// data/dispatch_policy.json（新增）
{
  "priority": ["buddy", "trae"],   // 池优先级列表（去 cheapest：两套积分体系倍率不可比）
  "per_model": {                   // 模型级覆盖，优先于 priority，键为 canonical_id
    "glm-5.3": ["trae", "buddy"]
  },
  "fallback": true,                // 双源模型首选池不可用时回退
  "updated_at": 1789000000
}
```

| 配置 | 语义 | 适用 |
|---|---|---|
| `priority` | 池优先级顺序数组，默认 `["buddy", "trae"]` | 显式化现状（Buddy 目录命中即 Buddy），可拖拽排序 |
| `per_model` | 单模型覆盖（canonical_id 键，值同为优先级数组） | 个别模型想固定走某池 |
| `fallback` | 双源模型首选池不可用时按序回退 | 关闭后仅用首选池 |

> 设计说明：v1 的 `cheapest`（按倍率跨池择优）已移除——Trae 通用积分与 Buddy 积分是两套独立体系，倍率数字不可直接比较，跨池"最省"无真实语义。顶层目录条目的 `rate` 展示**当前策略实际命中来源侧**的倍率（§3.1）。

### 4.3 错误处置矩阵（换号 / 换池 / 透传）

| 错误形态 | 处置动作 | 说明 |
|---|---|---|
| 网关鉴权失败（无效/欠额 Key） | 直接透传 401/402 | 不涉及上游 |
| 客户端参数错误（400 语义） | 直接透传 | 不换号、不换池 |
| 上游 401 凭证失效（Buddy） | 池内刷新（每账号一次）→ 换号 | 现有逻辑 |
| 上游 401（Trae JWT） | 换号 | 现有逻辑 |
| 429 限频 / 积分不足 | 该账号冷却（note_error）→ 换号 | 现有逻辑 |
| 5xx 上游错误 | 池内换号（MAX_ROTATE=3 内） | 现有逻辑 |
| 池耗尽（无健康账号 / 全冷却） | 双源 → 跨池回退；单源 → 503 no healthy account | 回退记 warn 日志（§4.5） |
| Buddy 模型级冷却中（model_cooldowns） | 双源 → 跨池回退；单源 → 429 显式报错 | 回退记 warn 日志 |
| 资源池关闭（wb_enabled=false） | 不参与选池（③ 已剔除）；仅 Buddy 源模型 → 显式报错（保持现有 wb_upstream_disabled 语义） | "关闭"与"不可用"是两个概念 |

### 4.4 池间回退与会话粘性

- **回退粒度**：按请求即时判定，不预绑定会话到池；
- **会话池粘性（软粘，防抖动）**：内存态 `session_key → (pool, expire_ts)`，TTL 60s；命中时优先沿用上次**成功**服务的池，池再次故障仍可实时回退。session_key 复用现有粘性会话键（conversation 维度），实现挂在 `ApiSharedState`，不落盘、重启即清；
- **与 Buddy 账号粘性的关系**：账号级粘性（wb_sticky_sessions）只在 Buddy 池内生效；某次请求回退到 Trae 池时**不写** Buddy 账号粘性绑定；
- **回退不清理**首选池的账号冷却状态（那是池内自治）。

### 4.5 指标与可观测性

- **当前并发数（inflight）**：`ApiSharedState` 增原子计数器 + **RAII guard**（Drop 时减计数，panic / 客户端断连 / 流异常终止均兜底释放）；覆盖**全部业务端点**：`/v1/chat/completions`、`/v1/completions`、`/v1/messages`、`/v1/responses`、`/v1/images/generations`、`/v1/images/edits`；
- `api_status` 返回体新增 `inflight` 字段；
- **回退日志**：跨池回退发生时记 warn 级 app_log：`dispatch fallback: model={id} preferred={buddy} actual={trae} reason={no_healthy_account|model_cooldown}`，配合请求日志 `pool=` 字段（已落地，标识为 `trae / buddy`）可完整回答"这次为什么走了 Trae"。

---

## 5. 全局 API 管理弹窗

### 5.1 入口（Sidebar 左下角）

- **删除** Github 图标按钮；
- 在**作者博客之前**（原 Github 位置）新增 **API 管理**图标（`KeyRound`），点击打开全局弹窗；
- 顺序变为：API 管理 → 作者博客 → 系统设置 → 主题切换。

### 5.2 布局（Modal，max-w-5xl，任意应用视图可开，Tab 分区）

v1 单屏堆叠 8 个区块过长，且 API Keys 管理自带子弹框会形成 Modal-in-Modal。v1.1 改为 **Tab 分区**：

```
┌────────────────────────────────────────────────────────────┐
│ API 管理                                                     │
│ OpenAI / Anthropic 兼容接口，通过资源池智能调度实现多账号负载均衡  [启动服务] │
│  运行状态   总请求数   当前并发数   API Key 数量                  │
├────────────────────────────────────────────────────────────┤
│ [概览]   [接口配置]   [API Keys 管理]   [用量统计]               │
├────────────────────────────────────────────────────────────┤
│ Tab · 概览：目前资源（双池摘要） │ 生态接入（统一注册）             │
│ Tab · 接口配置：端口 / 默认模型 / 上游说明 / 使用示例（读写 gateway 设置）│
│ Tab · API Keys 管理：列表 + 子 Key 调度配置 + 新建/编辑/删除        │
│ Tab · 用量统计：7/14/30 天 · 资源池筛选（全部/Trae/Buddy）· 模型分布 Top5 │
└────────────────────────────────────────────────────────────┘
```

- 默认落在「概览」Tab；指标行常驻头部，切 Tab 不消失；
- **子弹框层级**：Keys 管理 Tab 内的子 Key 配置弹框、删除确认仍用独立 `Modal` 组件，z 序高于管理弹窗（叠加弹层），不做内嵌折叠；
- **豆包视图说明**：弹窗在豆包应用视图同样可开，「概览」Tab 顶部提示「豆包不提供网关资源，以下为 Trae / Buddy 资源池」。

### 5.3 内容迁移映射

| 弹窗 Tab | 迁移来源 |
|---|---|
| 头部启停 + 指标行（常驻） | ApiService 服务状态卡 + BuddyApiService 网关状态卡（合并为一份） |
| 接口配置 Tab | ApiService 接口配置卡（端口/默认模型/使用方式与示例），改读写 `api_gateway_settings.json`（§8.1） |
| API Keys 管理 Tab | ApiService API Keys 管理卡（原样迁移，网关共享 Key 本就全局） |
| 概览 Tab · 目前资源 | ApiService 账号池状态摘要 + BuddyApiService WB 池状态（压缩为只读摘要，详情去资源调度页） |
| 概览 Tab · 生态接入 | 双页生态接入合并为统一注册（见 §7） |
| 用量统计 Tab | 双页用量统计合并：days + wb_days 双桶聚合，资源池筛选；含模型分布 Top 5（聚合双桶） |

### 5.4 组件化

从 ApiService.tsx / BuddyApiService.tsx 提取公共组件至 `src/components/api/`：

- `GatewayHeader`（启停 + 指标行，含 inflight）
- `InterfaceConfig`（接口配置）
- `ApiKeysManager`（Key 管理，原样）
- `EcoAccess`（生态接入，统一版）
- `ResourceSummary`（目前资源，双池摘要）
- `UsageStatsPanel`（用量统计，参数化 pool 筛选，内部含模型分布）

弹窗状态入全局 store：`showApiManager: boolean`。

---

## 6. 资源调度页（Trae / Buddy，同构设计）

### 6.1 Trae · 资源调度（原 "Trae · API 服务" 改造）

```
┌────────────────────────────────────────────────────────────┐
│ Trae · 资源调度                                               │
│ 服务资源池管理 · 账号调度与模型目录                              │
├────────────────────────────────────────────────────────────┤
│  积分体系说明                                                  │
│  通用积分（product_id 208）：签到 / 活动获取，各模型按倍率消耗；    │
│  账号池轮换消耗各账号积分，冷却与禁用规则同现有实现                 │
├────────────────────────────────────────────────────────────┤
│  可用账号数    活跃账号    池内账号                               │
├────────────────────────────┬───────────────────────────────┤
│  账号池选择                  │  模型目录（Trae）                 │
│  （enabled_uids / 策略 /     │  [同步官网模型] [编辑元数据]      │
│    分组过滤，现有迁移）        │  表：模型 ID · 展示名 · 积分倍率 ·  │
│                            │  思考档位 · 上下文 · 图片           │
│                            │  行内"编辑"入口 → 元数据弹框         │
└────────────────────────────┴───────────────────────────────┘
```

指标定义：
- **可用账号数**：健康且未冷却、可被调度选中的账号；
- **活跃账号**：近期（当日）被调度使用过的账号；
- **池内账号**：enabled_uids 总数。

模型目录数据：四层来源（§3.2：人工维护 > 官网同步 > 文档参考 > 名称推断），未确定字段显示 `—`。

**模型元数据编辑**（人工维护，L1）：
- 模型目录行内「编辑」→ 弹框可修改：展示名、积分倍率、思考档位（多选 low/medium/high）、上下文长度、图片支持；
- 保存落盘 `data/trae_model_meta.json` 覆盖层，**官网同步不覆盖人工值**；
- 已人工维护的条目显示编辑标记（如 `*`），可一键清除恢复自动来源；
- Buddy 侧模型目录沿用现有目录同步/自定义能力，同一编辑交互（落盘 `wb_model_catalog.json` 条目字段，语义一致）。

### 6.2 Buddy · 资源调度（原 "Buddy · API 服务" 改造）

同构布局，差异点：

| 区块 | Buddy 版内容 |
|---|---|
| 积分体系说明 | Buddy 积分：签到 / 成长中心获取，按模型倍率消耗 |
| 指标 | Buddy 池口径（wb_pool 的健康/使用/总数） |
| 资源开关卡（Buddy 独有，置于账号池选择之上） | `wb_enabled` 上游总开关 / 默认深度思考 / 工具执行 / 后台任务降级 |
| 账号池选择 | Buddy 账号池（现有 WB 池状态卡迁移） |
| 模型目录 | Buddy 目录（wb_model_catalog，含同步与自定义），字段：模型 ID · 展示名 · 积分倍率 · 思考档位 · 上下文 · 图片 |

> 迁出项：Buddy 页原"网关状态/启停/使用方式/生态接入/用量统计"全部迁入全局弹窗，页内不再出现网关级内容。

### 6.3 菜单更名

两侧 Sidebar 菜单项 `API 服务` → `资源调度`（icon 沿用，或换 `Layers`），页面标题同步为 `Trae · 资源调度` / `Buddy · 资源调度`。

---

## 7. 生态接入统一

- **CC Switch 注册**：只注册一个统一网关条目——provider id `aiwork-gateway-{app}`（沿用现有 Trae 侧 id），名称「AI Work 助手网关」，默认模型取网关 `api_default_model`（统一目录内模型）；
- **Codex toml**：同上，单条目；
- **兼容**：后端 `ccswitch_register` 的 `side` 参数保留（老调用不破坏），前端不再暴露双入口；CC Switch 中历史双条目（`aiwork-wb-gateway-*`）不主动删除，弹窗说明文字提示可手动清理；
- **注册说明**：备份机制（~/.cc-switch/backups/）与"仅写本网关条目"语义不变。

---

## 8. 数据与命令变更清单

### 8.1 新增

| 项 | 说明 |
|---|---|
| `api_unified_models(available_only?)` 命令 | 聚合 api_models + wb_catalog 去重视图，附 sources[].enabled 派生标记 |
| `canonical_id()` 工具函数 | trim + 小写；统一目录归并键 / per_model 键 / trae_model_meta 覆盖层键三处共用 |
| `dispatch_policy_get / dispatch_policy_set` 命令 | 调度策略读写（priority 数组 + per_model + fallback） |
| `data/dispatch_policy.json` | 策略落盘（缺失回退默认 `["buddy","trae"]`，池标识取值 `trae`/`buddy`） |
| `data/api_gateway_settings.json` + `gateway_settings_get/set` 命令 | **网关设置独立归属**：`port / default_model`；启动时若新文件缺失且 app_settings.json 存在旧字段 → 一次性迁移（旧字段保留不删，防回滚，但不再读取） |
| `ModelOption` 扩展（`models_sync.rs`） | 条目增 `rate / context_length / efforts / supports_image`（serde default 兼容旧文件）；`parse_official` 扩展解析响应中的倍率等运营字段 |
| `data/trae_model_meta.json` | 人工维护覆盖层（L1，键 canonical_id），同步不覆盖 |
| `trae_model_meta_set / trae_model_meta_clear` 命令 | 元数据编辑/清除（编辑弹框读写） |
| `ApiSharedState.inflight` + RAII guard | 当前并发计数（§4.5，覆盖 6 个业务端点） |
| 会话池粘性（内存态） | `session_key → (pool, expire_ts)`，TTL 60s（§4.4），不落盘 |
| 跨池回退 warn 日志 | `dispatch fallback: model=… preferred=… actual=… reason=…`（§4.5） |
| `store.showApiManager` + `ApiManagerModal` 组件 | 全局弹窗（Tab 分区） |
| `fs_utils::read_json_cached` 解析缓存 | 调度热路径 4 份数据文件（dispatch_policy / wb_model_route / wb_model_catalog / api_models）按 mtime+size 校验缓存已解析值，`write_json` 写后逐出——消除 `resolve_target` 每请求约 4 次磁盘读（v1.2 修订） |

### 8.2 修改

| 项 | 变更 |
|---|---|
| `routes.rs` 分流点 | `resolve_wb_target` → `resolve_target`（canonical_id → sources（剔除 disabled）→ 会话粘性/策略选池 → 错误矩阵处置） |
| `GET /v1/models` | 返回统一目录（合并去重 + sources；wb_enabled=false 时过滤仅 Buddy 源模型） |
| `api_status` | 增 `inflight` |
| usage 查询 | 保留 days / wb_days 分桶；聚合展示由前端合并（弹窗筛选"全部"时相加） |
| 网关启动读取（`do_start`） | api_port / api_default_model 改读 `api_gateway_settings.json` |
| `ApiService.tsx` | 拆解：公共部分提取组件，资源部分留页 |
| `BuddyApiService.tsx` | 拆解同上 |
| `Sidebar.tsx` | 删 Github、增 API 管理入口、菜单更名 |

### 8.3 不变（明确保留）

- `api_keys.json` / `api_pool.json`（Trae 字段与 wb_* 字段共存）/ `wb_model_catalog.json` / `wb_model_route.json` 四段管线 / `wb_sticky_sessions.json` / 双桶记账语义；
- Trae 池与 WB 池的池内调度算法；
- 三协议端点与鉴权行为。

---

## 9. 迁移与兼容

1. **行为兼容**：默认策略 `["buddy", "trae"]` 下，请求路由结果与现状一致（现状即"Buddy 目录命中即 Buddy"），重构为显式配置后零行为差异；
2. **数据零迁移**：唯一例外是网关设置（§8.1 `api_gateway_settings.json`，启动时一次性从 app_settings 抽取，旧字段保留防回滚）；其余全部派生/可选，旧数据文件直接复用；
3. **Breaking change 记录**：`GET /v1/models` 的 `owned_by` 由 `trae-solo / workbuddy` 变为 `unified`——依赖旧值的脚本需同步（本生态内已知消费者仅网关自身与调试用途）；
4. **无双写期**：Phase 2 弹窗上线时，旧 API 服务页的网关级卡片同步**只读化**（附「已迁移至全局 API 管理」引导），不保留可编辑双入口，避免端口/默认模型双写竞态；
5. **回滚安全**：前端组件提取为纯搬移（props 化），后端策略层有默认值，逐 Phase 可独立回滚；
6. **Trae 历史数据兼容清单**（Trae 侧必须无损，逐项验收）：
   - `api_models.json` 存量条目（仅 `id/label`）：`ModelOption` 新字段全部 `serde default` 可选，旧文件原样读取，不重写不丢失；
   - `GET /v1/models` 响应保持 OpenAI 标准形状（`data[].id` 列表），`sources / owned_by` 为**附加**字段——存量 Trae 生态客户端（CC Switch 条目接入的 Claude Code / Codex）解析不受影响；
   - `days` 桶历史用量：口径与键名不变，统一统计聚合时不重算历史；
   - Trae 侧 CC Switch 存量条目 id（`aiwork-gateway-*`）不变；
   - Trae 官网模型名透传语义不变（`glm-5.2` 等官网模型在默认策略下行为与现状一致）；
   - `app_settings.json` 中 `api_port / api_default_model` 旧字段保留（迁移后不删，防回滚）；
7. **Buddy 侧不保历史**：`wb_days` 用量桶、`wb_model_catalog.json` 格式、粘性会话数据若随实施调整格式，**无需兼容迁移**——必要时清空重来，不承担存量数据包袱（账号凭证文件 `workbuddy_accounts.json` 除外，属用户资产，任何改动必须无损）；
8. **文档同步**：user-manual.md 的 API 服务章节需随 Phase 3 更新。

---

## 10. 实施计划

### Phase 1：后端基础（统一目录 + 调度策略 + 指标）

- 统一模型目录聚合（`api_unified_models` + `/v1/models` 合并 + wb_enabled 过滤）；
- Trae 元数据链路：`ModelOption` 扩展 + `parse_official` 倍率解析（先抓包确认响应字段名）+ L3 文档参考表（Max 模式 1M 上下文等）+ L4 名称推断规则 + `trae_model_meta.json` 覆盖层与命令；
- `dispatch_policy.json` + `resolve_target` 双池调度（错误矩阵处置 + 会话池粘性 + 回退 warn 日志）；
- `api_gateway_settings.json` 设置归属迁移（do_start 改读）；
- `inflight` 并发计数（RAII guard，6 端点）+ `api_status` 增字段。

**验收**：cargo test 全过，覆盖以下**调度测试矩阵**（≥ 18 用例）：

| 维度 | 取值 |
|---|---|
| 优先级 | `["buddy","trae"]` / `["trae","buddy"]` / per_model 覆盖 |
| 模型源 | 双源 / 仅 Trae / 仅 Buddy |
| 池状态 | 健康 / 耗尽（无健康账号）/ 模型冷却中 / wb_enabled=false |

关键断言：默认策略下路由行为与改造前一致（对比用例）；wb_enabled=false 时仅 Buddy 源模型显式报错、双源模型走 Trae；回退发生时写 warn 日志；人工维护值在官网同步后保留；inflight 在 panic 路径不泄漏（guard 单测）。

### Phase 2：全局 API 管理弹窗（Tab 化）

- 提取公共组件（§5.4）；
- `ApiManagerModal`（Tab 分区：概览/接口配置/Keys 管理/用量统计）+ `store.showApiManager`；
- Sidebar：删 Github、增 API 管理入口；
- **旧 API 服务页网关级卡片同步只读化**（无双写期，§9.4）。

**验收**：tsc / build / vitest 全过；弹窗在三个应用视图均可打开（豆包视图含说明文案）；接口配置改端口后网关重启生效（读写 `api_gateway_settings.json`）；Key 增删 / 用量统计 / 生态注册与迁移前功能等价（回归清单：Key 新建→编辑子 Key 调度→删除；用量 7/14/30 切换与池筛选；CC Switch 注册后 toml 条目正确）。

### Phase 3：资源调度页改造 + 生态统一 + 清理

- Trae / Buddy 页改造为资源调度页（§6）；菜单与标题更名；
- Trae 模型目录元数据编辑功能（行内编辑 → 弹框 → `trae_model_meta.json`）；
- 生态接入统一为单条目注册；移除双页旧网关入口（只读卡一并清除）；
- 文档更新（user-manual）。

**验收**：全量验证（tsc / build / vitest / cargo test / cargo check 零警告）；三应用页面无网关级内容残留；CC Switch 新注册仅产生单条目；模型目录编辑保存后刷新页面保留、官网同步后人工值不被覆盖。

---

## 11. 风险与权衡（ADR 简记）

| 决策 | 权衡 | 结论 |
|---|---|---|
| 对外池标识 `buddy`、内部 `wb_*` 保留 | 全量改名统一 vs 纯重命名无收益且引入数据迁移风险 | 对外标识 buddy（目录/策略/日志），内部文件与字段保留 wb_ 前缀，聚合层做映射 |
| Trae 历史数据兼容、Buddy 不保历史 | 双侧都兼容成本高 | Trae 存量（模型目录/用量/CC 条目/响应形状）逐项无损验收；Buddy 侧格式可重置（账号凭证除外） |
| 统一目录不落盘（派生聚合） | 落盘可离线审计 vs 引入第三份需同步的数据 | 派生优先，源文件仍是唯一事实 |
| 去 `cheapest`，策略用池优先级列表 | 跨池倍率比价看似"智能" vs 两套积分体系不可比 | 用 `priority` 数组，语义直白可扩展 |
| Trae 元数据四层来源 | 接口字段名未抓包确认（倍率肯定在，字段名待定）；L3 参考表随官方文档漂移 | L2 解析失败自动落到 L3/L4 不阻塞；人工维护（L1）可修正一切 |
| 会话池粘性为内存态（TTL 60s） | 落盘可跨重启延续 vs 池状态本身易变、落盘引入陈旧绑定 | 内存态，重启即清，牺牲轻量延续换正确性 |
| 网关设置迁 `api_gateway_settings.json` | 留在 app_settings 少一次迁移 vs 归属与"公共网关"定位不符 | 独立归属 + 启动一次性迁移，旧字段保留防回滚 |
| Phase 2 无双写过渡期 | 双入口过渡更平滑 vs 端口/默认模型双写竞态 | 旧页网关卡片只读化，直接切换 |
| CC Switch 统一条目沿用 `aiwork-gateway-*` id | 老双条目残留 vs 强制清理用户数据 | 不删用户数据，说明提示手动清理 |
| 单源模型不跨池回退 | 静默换池可能改变积分消耗口径 vs 可用性 | 保持显式报错，用户可见可控 |
| `owned_by` 改 `unified`（breaking） | 保持旧值兼容 vs 单一网关语义统一 | 改，记录在 §9.3，生态内已知消费者不受影响 |
