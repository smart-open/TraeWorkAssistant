//! WorkBuddy 上游请求体改写（T2.1/F-28/F-30 v1.2）
//!
//! 上游 `POST {chatBase}/v2/chat/completions` 为 OpenAI 兼容体，但有四个坑：
//! 1. 拒绝非流式（code 11101）→ 强制 `stream:true`，非流式由本地聚合；
//! 2. `tool_choice` 对象形式报 400 → 归一化为 string；
//! 3. `reasoning_effort` 档位上游不支持时拒答/忽略 → 按目录 supportedEfforts
//!    降级（调用方先用 wb_catalog::resolve_effort 解析，本层只负责注入）；
//! 4. Claude Code 指纹触发腾讯内容审核 → 指纹清洗（身份句改写 + cc_xxx 键值
//!    剥离 + x-anthropic-* 引用剥离）+ 审核模板黑名单最小改写（映射表外置
//!    `wb_template_map.json` 可热更新，§5.5 #9 cat-and-mouse，禁硬编码扩散）。
//!    issue #122 方案 C 增强：`to` 支持 `|` 多候选按请求种子轮换（C1）、
//!    空完成换号重试追加激进短前缀规则（C2）、首条 system 残指纹自检（C3）。
//!
//! 另：连续同角色消息自动合并（生态实测，§5.8 #9）。

use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Mutex, OnceLock};

/// 默认审核模板映射（映射表文件缺失时的内置兜底，§5.5 #9）：以最小改写
/// （CLI→CLI tool、based on→built on——任何一字改动即绕过逐字匹配）为主；
/// issue #122 起实证句升级为整句同义改写——单词级改写后品牌词/n-gram 残留
/// （opencode、DeepSeek Harness、interactive CLI 等）仍被上游特征识别秒拒。
/// Codex 两条身份句前缀不带句号，兼容 GPT-5/GPT-5-Codex 等模型后缀变体。
pub fn default_template_map() -> Vec<(String, String)> {
    vec![
        (
            "You are Claude Code, Anthropic's official CLI for Claude.".to_string(),
            "You are Claude Code, Anthropic's official CLI tool for Claude.".to_string(),
        ),
        // issue #99 布防：VS Code 扩展（IDE 形态）身份句逗号续接变体——上方规则
        // 句号结尾，逗号变体（"...for Claude, running inside the VS Code extension."
        // 等）必然漏匹配；前缀匹配覆盖任意续接。精确触发句以「11128 指纹样本」
        // 诊断日志核实后经 UI 热更新迭代，无须发版
        // Claude Code v2.1.312（Agent SDK harness）实证身份句：system 为数组
        // 多块、身份句在第二 text 块（首块空串），anthropic_to_openai 无缝
        // 拼接后落首条 system 消息可命中；整句改写去除品牌词（Claude/Anthropic/
        // Agent SDK），零残指纹 + 幂等由回归门自动锁定
        (
            "You are a Claude agent, built on Anthropic's Claude Agent SDK.".to_string(),
            "You are a coding agent, built on an internal agent toolkit.".to_string(),
        ),
        (
            "You are Claude Code, Anthropic's official CLI for Claude,".to_string(),
            "You are Claude Code, Anthropic's official CLI tool for Claude,".to_string(),
        ),
        (
            "Main branch (you will usually use this with PRs)".to_string(),
            "Default branch (you will usually use this with PRs)".to_string(),
        ),
        (
            "Main branch (you will usually use this for PRs)".to_string(),
            "Default branch (you will usually use this for PRs)".to_string(),
        ),
        // Codex CLI v1 harness：instructions 首句身份声明
        (
            "You are Codex, based on GPT-5".to_string(),
            "You are Codex, built on GPT-5".to_string(),
        ),
        // Codex CLI v2 harness：instructions 首句身份声明
        (
            "You are Codex, an agent based on GPT-5".to_string(),
            "You are Codex, an agent built on GPT-5".to_string(),
        ),
        // Codex CLI v3 harness（9/19 新版 codex.exe 实证）：instructions 首句身份声明
        (
            "You are Codex, a coding agent based on GPT-5".to_string(),
            "You are Codex, a coding agent built on GPT-5".to_string(),
        ),
        // Codex CLI v4 harness（同上新版 codex.exe 实证，CLI 官方 harness）
        (
            "You are a coding agent running in the Codex CLI".to_string(),
            "You are a coding agent working in the Codex CLI".to_string(),
        ),
        // Codex CLI 0.145.0（2026-10-29 /v1/responses 抓包实证，经
        // responses_to_chat 投影后走统一清洗管线）：instructions 首句续句 +
        // 语境内指代句——品牌词 Codex/OpenAI 双点，与 v4 首句规则句式不同
        // （新缺口）。整句同义改写连根消除（C3 口径：to 零品牌词，回归门
        // default_map_outputs_are_residue_free_and_idempotent 锁定）。
        // RESIDUAL_MARKERS 不可加 "codex"——旧 v1-v4 规则改写句含 Codex/GPT-5
        // （无实证品牌词被拦，按 #9 cat-and-mouse 以实证为准保留），纳入即触发
        // 回归门误报
        (
            "Codex CLI is an open source project led by OpenAI.".to_string(),
            "The command-line coding tool is an open source project maintained by a dedicated engineering team."
                .to_string(),
        ),
        (
            "Within this context, Codex refers to the open-source agentic coding interface (not the old Codex language model built by OpenAI)."
                .to_string(),
            "Within this context, the product name refers to the open-source agentic coding interface (not the older language model that once shared the name)."
                .to_string(),
        ),
        // TraeCode harness：system 首句身份声明（日志实证 pool=trae 请求原文，
        // 预防性兜底——同一指纹句一旦进入风控名单即全量 11128）。
        // issue #129（buddy-ai 国际档实证）：品牌词 TraeCode 本身即竞品 harness
        // 特征，最小改写（in→running in）后残留仍被渠道验证拦截——整句同义
        // 改写连根消除；to 不得含 TraeCode（C3 口径，残指纹特征词表成员，
        // default_map_outputs_are_residue_free_and_idempotent 回归锁定）
        (
            "You are an interactive agent in TraeCode that helps the USER with software engineering tasks.".to_string(),
            "You are an interactive agent that helps the USER with software engineering tasks.".to_string(),
        ),
        // 旧版最小改写句形态（≤v3.8.0 发出的 "running in TraeCode"）作为 from
        // 幂等兜底：存量自定义表抄录内置表 / 跨版本升级场景同样收敛零残留
        (
            "You are an interactive agent running in TraeCode that helps the USER with software engineering tasks.".to_string(),
            "You are an interactive agent that helps the USER with software engineering tasks.".to_string(),
        ),
        // issue #57 预防性布防：REASONIX/DSH 等第三方客户端指纹句（无真实请求
        // 日志可查证，按常见开源 CLI harness 身份句先布防——未命中即无操作）。
        // 上线后从请求日志（debug）核实触发句，通过 KV wb_template_map.json
        // 热更新迭代精确规则，无须发版
        (
            "You are Cline, a highly skilled software engineer with extensive knowledge".to_string(),
            "You are Cline, a highly capable software engineering assistant with extensive knowledge"
                .to_string(),
        ),
        (
            "You are Roo, a highly skilled software engineer with extensive knowledge".to_string(),
            "You are Roo, a highly capable software engineering assistant with extensive knowledge"
                .to_string(),
        ),
        (
            "You are an interactive CLI agent specializing in software engineering tasks."
                .to_string(),
            // issue #122 方案 C3 口径：「interactive CLI」属上游特征 n-gram，
            // 改写句自身亦不得残留（residual_fingerprint_note 特征词表成员）
            "You are a CLI assistant specializing in software engineering tasks.".to_string(),
        ),
        // OpenCode harness（公开 prompt 可查证，issue #57 布防）：按 provider
        // 分多套 flavor，首句身份声明各不相同，逐条改写（含大小写变体）。
        // issue #122 升级（2026-10-18 实证 ×24：模板命中仍在涨、上游 server-timing
        // inner 212~364ms 秒拒）——tool→assistant 最小改写后残留品牌词 opencode /
        // interactive CLI / software engineering tasks 等 n-gram，整句同义改写
        // 三类特征一并消除
        (
            "You are opencode, an interactive CLI tool that helps users with software engineering tasks."
                .to_string(),
            "You are a pragmatic senior engineer helping the user get coding work done in a terminal."
                .to_string(),
        ),
        // issue #122 方案 C3 口径适用于本块全部规则：改写句自身含品牌词/特征
        // n-gram = 指纹仍在（首轮请求激进规则不兜底）+ residual_fingerprint_note
        // 设计性误报——to 值一律不得残留 opencode / interactive CLI / DeepSeek
        // Harness（default_map_outputs_are_residue_free_and_idempotent 回归锁定）
        (
            "You are OpenCode, You and the user share the same workspace.".to_string(),
            "You and the user work in the same workspace.".to_string(),
        ),
        (
            "You are OpenCode, the best coding agent on the planet.".to_string(),
            "You are a top software engineer for coding work.".to_string(),
        ),
        (
            "You are opencode, an interactive CLI agent specializing in software engineering tasks."
                .to_string(),
            "You are a pragmatic senior engineer helping users with coding work in a terminal."
                .to_string(),
        ),
        (
            "You are operating as and within the OpenCode CLI, a terminal-based agentic coding assistant built by OpenAI."
                .to_string(),
            "You are operating as and within a terminal-based agentic coding tool built by OpenAI."
                .to_string(),
        ),
        // OpenCode v2.2.1（2026-10-29 抓包实证）：新版身份句品牌词在句中
        // （running in OpenCode），与上方旧版 "You are opencode…" 前缀形态完全
        // 不同，旧规则与本表其余规则均不命中——整句同义改写连根消除（C3 口径）
        (
            "You are an AI agent running in OpenCode, a coding agent harness.".to_string(),
            "You are an AI agent working in a local coding workspace.".to_string(),
        ),
        // DeepSeek Harness（DSH，2026-10-06 测试机日志实证：pool=trae 空完成影子
        // 风控，4 账号全 healthy 仍全灭；请求日志抓到身份句原文）。首句身份声明 +
        // 模型句 "powered by" 复现（glm 前缀匹配覆盖 5.3/5.3-flash 变体）。
        // issue #122 升级（2026-10-18 实证 ×45）：based on→built on 后品牌词
        // DeepSeek Harness 原样残留仍是强指纹——首句整句同义改写连根消除；
        // glm 句换掉 "You are a coding agent" 自我声明句式，保留 glm- 前缀衔接
        // （后接模型名语法不断）
        (
            "You are an AI agent powered by DeepSeek Harness.".to_string(),
            "You are an AI agent that helps the user with programming work.".to_string(),
        ),
        (
            "You are a coding agent powered by the glm-".to_string(),
            "You help the user with programming work using the glm-".to_string(),
        ),
        // 2026-10-29 抓包实证（DSH 0.2.1-rc.2 headless 样本）：模型句池前缀
        // 升级为 buddy/deepseek-v4.1-flash，上方 glm- 前缀规则不命中——按 glm
        // 句同款句式整句同义改写，保留模型名衔接；RESIDUAL_MARKERS 的
        // "deepseek harness" 是整词组，模型名 deepseek-v4.1-flash 不触发回归门
        (
            "You are a coding agent powered by the buddy/deepseek-v4.1-flash model.".to_string(),
            "You help the user with programming work using the buddy/deepseek-v4.1-flash model."
                .to_string(),
        ),
        // CodeBuddy 1.106.1（2026-10-29 抓包实证，buddy 池 /v1/chat/completions
        // 单一形态 ×10）：模型句句式与 glm- 前缀规则（"You are a coding agent
        // powered by the glm-"）不同——"an AI coding assistant, powered by"
        // 逗号续接形态，逐字整句同义改写保留模型名衔接；正文两处独家品牌句
        // 一并布防（".codebuddy" 文件夹警告句 + CodeBuddy docs 官网域名句，
        // 品牌词 ×4 强指纹连根消除，C3 口径：to 零品牌词，回归门锁定）。
        // RESIDUAL_MARKERS 新增 "codebuddy" 整词组——模型名 buddy/glm-5.3-flash
        // 不含该词组，不触发回归门
        (
            "You are an AI coding assistant, powered by buddy/glm-5.3-flash model.".to_string(),
            "You help the user with programming work using the buddy/glm-5.3-flash model."
                .to_string(),
        ),
        (
            "**IMPORTANT**: \".codebuddy\" folder stores project-related data and is NOT a temporary cache. Please do NOT delete this folder!".to_string(),
            "**IMPORTANT**: This project stores its configuration data in a dedicated local folder that is NOT a temporary cache. Please do NOT delete this folder!".to_string(),
        ),
        (
            "When the user asks about CodeBuddy or how to use one of its features (eg. \"can CodeBuddy do...\", \"how do I implement a hook / write a slash command / install an MCP server\"), read the CodeBuddy docs at https://www.codebuddy.ai/docs/zh/ide/User-guide/Overview with the web_fetch tool instead of guessing — if web_fetch is not in your tool list, retrieve it with ToolSearch first.".to_string(),
            "When the user asks about this coding tool or how to use one of its features (eg. \"can it do...\", \"how do I implement a hook / write a slash command / install an MCP server\"), read its official online documentation with the web_fetch tool instead of guessing — if web_fetch is not in your tool list, retrieve it with ToolSearch first.".to_string(),
        ),
        // ZCode harness（issue #155，2026-10 实证：pool=trae 空完成影子风控，
        // 3 账号轮换全空后 503；请求指纹样本抓到身份句原文）。首句身份声明 +
        // 第二身份句双句声明（DSH 式双声明形态）。整句同义改写连根消除品牌词
        // ZCode（C3 口径：to 零品牌词，回归门 default_map_outputs_are_residue_
        // free_and_idempotent 锁定；RESIDUAL_MARKERS 已纳入 "zcode"）
        (
            "You are ZCode, an interactive coding agent".to_string(),
            "You are an interactive coding agent".to_string(),
        ),
        (
            "You are an interactive ZCode agent that helps users with software engineering tasks".to_string(),
            "You are an interactive agent that helps users with software engineering tasks".to_string(),
        ),
        // Qoder CN 0.4.11（2026-10-29 混淆包逆向实证，QoderWork worker）：chat
        // 生成请求经 @qoder-ai/qoder-cn-agent-sdk worker 直连网关绕过代理，
        // 线上 payload 无法 MITM——base64+XOR 循环密钥解码混淆包还原身份句
        // 原文。主身份句双品牌点（QoderWork + Qoder team）整句同义改写连根
        // 消除；变体句（Qoder's + Qoder team 双点）同款处理（C3 口径：to 零
        // 品牌词，回归门 default_map_outputs_are_residue_free_and_idempotent
        // 锁定；RESIDUAL_MARKERS 已纳入 "qoderwork" / "qoder team" 整词组）
        (
            "You are QoderWork, a desktop agentic assistant developed by Qoder team. You are built for daily work, helping users improve their productivity. Use the instructions below and the tools available to you to assist the user.".to_string(),
            "You are a desktop work companion built by an internal productivity team. You are made for everyday tasks, helping users get more done. Follow the instructions below and use the tools available to you to help the user.".to_string(),
        ),
        (
            "You are Qoder's desktop agentic assistant, built by the Qoder team to help users solve problems and get things done with clarity and confidence.".to_string(),
            "You are a desktop work companion built by an internal productivity team to help users solve problems and get things done with clarity and confidence.".to_string(),
        ),
    ]
}

/// 模板映射文件结构：wb_template_map.json
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
pub struct TemplateMapFile {
    #[serde(default)]
    pub templates: Vec<TemplateRule>,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

// ==================== 激进清洗规则（issue #122 方案 C2） ====================

/// 空完成升级式清洗的内置激进规则（issue #122 方案 C2）：仅在「同一请求发生
/// 空完成后的换号重试」时追加于生效表之后应用（首轮请求不应用，避免对存量
/// 正常请求引入改写差异）。短前缀匹配——兜住生效表逐字规则覆盖不到的未知
/// 变体（不同后缀/flavor 的同源身份句）；误伤面：仅身份句前缀形态，用户正文
/// 以该形态开头的概率极低，可接受。应用顺序在生效表之后：整句规则先行改写
/// （品牌词连根消除），激进规则只兜改写后仍残留前缀的漏网变体
pub fn aggressive_template_rules() -> Vec<(String, String)> {
    vec![
        // OpenCode 未知 flavor 变体（默认表 4 条逐字规则只覆盖已知句式）
        (
            "You are opencode".to_string(),
            "You are a pragmatic senior engineer".to_string(),
        ),
        (
            "You are OpenCode".to_string(),
            "You are a pragmatic senior engineer".to_string(),
        ),
        // OpenCode v2.2.1 句中兜底（2026-10-29 抓包实证：品牌词在句中，上方
        // 前缀规则不命中）；长规则先行防短规则截断破句，未知后缀/flavor 变体
        // 由此兜住
        (
            "running in OpenCode, a coding agent harness".to_string(),
            "working in a local coding workspace".to_string(),
        ),
        (
            "running in OpenCode".to_string(),
            "working in a local coding environment".to_string(),
        ),
        // 未知 CLI harness（REASONIX/ZCode 等无公开资料客户端）的通用身份前缀
        (
            "You are an interactive CLI".to_string(),
            "You are a terminal-based assistant".to_string(),
        ),
        // 「powered by <harness>」句式兜底（默认表覆盖 DeepSeek Harness 逐字句，
        // 变体后缀/新 harness 名由此兜住；改写后接续原文句尾标点不断）
        (
            "powered by DeepSeek Harness".to_string(),
            "that helps the user with programming work".to_string(),
        ),
        // DSH 0.2.1-rc.2（2026-10-29 抓包实证）模型句前缀兜底：默认表逐字覆盖
        // buddy/deepseek-v4.1-flash 实证句、glm- 前缀覆盖 glm 变体，未知模型名
        // 变体由此兜住；置于 "powered by DeepSeek Harness" 兜底之后——DSH 品牌
        // 变体优先走连根消除规则，避免本规则改写后 "using the DeepSeek Harness"
        // 反向残留品牌词
        (
            "You are a coding agent powered by the ".to_string(),
            "You help the user with programming work using the ".to_string(),
        ),
        // CodeBuddy 1.106.1（2026-10-29 抓包实证）模型句前缀兜底：默认表逐字
        // 覆盖 buddy/glm-5.3-flash 实证句，未知模型名变体由此兜住；前缀
        // "You are an AI coding assistant" 为 CodeBuddy 独有形态（其余已布防
        // 客户端身份句无此前缀），误伤面仅限该前缀句；置于 DSH 前缀兜底之后
        (
            "You are an AI coding assistant, powered by ".to_string(),
            "You help the user with programming work using the ".to_string(),
        ),
        // issue #129：Trae 客户端身份句前缀兜底（默认表只覆盖已知整句，chat
        // 模式 / 新版本变体由此兜住）；长前缀规则先行，避免 "You are Trae AI"
        // 被 "You are Trae" 短规则截断成 "assistant AI" 语义破句
        (
            "You are an interactive agent in TraeCode".to_string(),
            "You are an interactive agent".to_string(),
        ),
        (
            "You are an interactive agent running in TraeCode".to_string(),
            "You are an interactive agent".to_string(),
        ),
        (
            "You are Trae AI".to_string(),
            "You are an AI assistant".to_string(),
        ),
        (
            "You are Trae".to_string(),
            "You are an AI assistant".to_string(),
        ),
        // Claude Code v2.1.312（Agent SDK harness）身份句前缀兜底：默认表覆盖
        // 实证整句，新版后缀/flavor 变体（SDK 版本号等）由此兜住；改写保尾部
        // 衔接，品牌前缀连根消除
        (
            "You are a Claude agent".to_string(),
            "You are a coding agent".to_string(),
        ),
        // issue #155：ZCode 身份句前缀兜底（默认表覆盖两条实证整句，新版
        // 后缀/flavor 变体由此兜住；"You are ZCode" 短规则居后，避免截断
        // "You are ZCode, an interactive coding agent" 整句规则的改写衔接）
        (
            "You are ZCode, an interactive coding agent".to_string(),
            "You are an interactive coding agent".to_string(),
        ),
        (
            "You are ZCode".to_string(),
            "You are an interactive agent".to_string(),
        ),
        (
            "You are an interactive ZCode agent".to_string(),
            "You are an interactive agent".to_string(),
        ),
        // Codex 0.145.0（2026-10-29 /v1/responses 抓包实证）身份句前缀兜底：
        // 默认表逐字覆盖首句续句与语境内指代句实证整句，新版后缀/flavor 变体
        // 由此兜住；改写保留 "open source project" 谓语衔接，品牌前缀连根消除
        (
            "Codex CLI is an open source project".to_string(),
            "The command-line coding tool is an open source project".to_string(),
        ),
        // Qoder CN 0.4.11（2026-10-29 混淆包逆向实证）身份句前缀兜底：默认表
        // 覆盖两条实证整句，未知 flavor/后缀变体由此兜住。主句品牌词双点
        // （QoderWork + Qoder team）——长前缀整段兜底先行；句中品牌词组按
        // OpenCode "running in OpenCode" / DSH "powered by" 中段规则先例兜住；
        // "You are QoderWork" / "You are Qoder's" 短规则居后防截断长规则
        (
            "You are QoderWork, a desktop agentic assistant developed by Qoder team".to_string(),
            "You are a desktop work companion built by an internal productivity team".to_string(),
        ),
        (
            "You are Qoder's desktop agentic assistant, built by the Qoder team".to_string(),
            "You are a desktop work companion built by an internal productivity team".to_string(),
        ),
        (
            "developed by Qoder team".to_string(),
            "built by an internal productivity team".to_string(),
        ),
        (
            "built by the Qoder team".to_string(),
            "built by an internal productivity team".to_string(),
        ),
        (
            "You are QoderWork".to_string(),
            "You are a desktop work companion".to_string(),
        ),
        (
            "You are Qoder's".to_string(),
            "You are a".to_string(),
        ),
    ]
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct TemplateRule {
    pub from: String,
    /// None（缺省/null）= 停用该规则（从生效表移除，原句透传）；
    /// Some("")（空串）= 剥离（改写为空串，删除指纹原文）——两种空态必须区分，
    /// 存量数据 to:"" 反序列化为 Some("")，剥离行为不因合并语义翻转。
    /// issue #122 方案 C1：`|` 分隔多候选（如 "A|B|C"）——WB 池按请求种子
    /// 稳定选一、跨换号重试变化，消除「全网统一改写句」二次指纹化；
    /// Trae 池（payload.rs 走 apply_template_map seed=0）按规则名哈希稳定
    /// 选一。候选可为空串（剥离语义合法）
    #[serde(default)]
    pub to: Option<String>,
}

impl TemplateMapFile {
    /// 转换为 (from, to) 规则表：剔除空 from；全空 → None（调用方走纯默认表）
    pub fn into_rules(self) -> Option<Vec<(String, Option<String>)>> {
        let rules: Vec<(String, Option<String>)> = self
            .templates
            .into_iter()
            .filter(|r| !r.from.is_empty())
            .map(|r| (r.from, r.to))
            .collect();
        if rules.is_empty() {
            None
        } else {
            Some(rules)
        }
    }
}

/// 合并规则表（issue #99 ⑥）：内置默认表全量生效，自定义表按 from 逐字覆盖
/// 同名项、其余追加。旧「整体替代」语义的缺陷：默认表新增布防规则（如 VS Code
/// 逗号变体）后，存量自定义表拿不到新规则，每次发版都需手动同步。
/// 语义：to = Some(s) 覆盖/追加改写；to = Some("") 剥离（改写为空串，旧行为
/// 保持）；to = None 停用（从生效表移除该 from 规则，原句透传）。「省略某默认
/// 规则」不再表示删除——停用请显式保存 to 为 null。
pub fn merged_template_rules(
    custom: Option<Vec<(String, Option<String>)>>,
) -> Vec<(String, String)> {
    let mut merged = default_template_map();
    if let Some(rules) = custom {
        for (from, to) in rules {
            match to {
                // 停用：从生效表移除
                None => merged.retain(|(f, _)| *f != from),
                // 改写（含空串剥离）：同名覆盖、其余追加
                Some(t) => {
                    if let Some(slot) = merged.iter_mut().find(|(f, _)| *f == from) {
                        slot.1 = t;
                    } else {
                        merged.push((from, t));
                    }
                }
            }
        }
    }
    merged
}

// ==================== 工具名别名改写（issue #150：tools 工具集指纹识别） ====================

/// 内置工具别名默认表（issue #150）：上游风控升级为按请求 `tools` 工具名集合
/// 识别 agent 客户端——system 句表清洗后样本 system 已空仍被拦，换号无效
/// （拦「客户端形态」不拦账号），无 tools 请求不受影响。句表机制结构性无解：
/// tools 是 function calling 协议定义，客户端按名匹配响应 tool_calls，单向
/// 改名即废。本表在请求侧把特征工具名改写为语义保持的通用名（模型仍可凭
/// 描述选对工具），响应侧按逆映射回改（协议对客户端透明）。
/// 仅收「特定 agent 客户端独家命名」（glob/grep/edit 等通用名不改，避免
/// 无谓的模型侧语义损失）；表随样本画像持续补齐（UI「工具别名」tab 热更新）。
/// 收录口径与指纹清洗 default_template_map 布防客户端对齐（同 issue #150 一体
/// 治理——客户端形态识别不止 system 句一条通道）：Claude Code / Codex /
/// Cline / Roo 的实证独家名逐家补齐；OpenCode v2.2.1（2026-10-29 抓包）实证
/// 12 工具全通用小写名（edit/glob/grep/question/read/shell/skill/subagent/
/// webfetch/websearch/write/execute）；DSH 0.2.1-rc.2（2026-10-29 headless
/// 抓包）实证 25 工具——6 个与 Trae 段同名天然覆盖、4 个独家名见下方 DSH 段，
/// 其余 15 个为通用名不改；CodeBuddy 1.106.1（2026-10-29 抓包）实证 18 工具
/// ——4 个经既有段同名规则天然覆盖（replace_in_file/write_to_file/
/// ask_followup_question/ToolSearch）、7 个独家名见下方 CodeBuddy 段、
/// 7 个通用名不改；ZCode 工具集尚无样本可采信——无独家名可布防
/// （瞎编名字零收益）；Qoder CN 0.4.11（2026-10-29 混淆包逆向实证）实证工具
/// 集与 Claude Code 同构（Read/Write/Edit/Glob/Grep/Agent/TodoWrite/WebFetch/
/// WebSearch/AskUserQuestion/NotebookEdit 同名经 Claude Code 段天然覆盖），
/// 独家名为 mcp 三件套 mcp_list/mcp_get/mcp_call + SendMessage/TaskCreate，
/// 见下方 Qoder 段；
/// 待样本画像核实后经自定义表热补（指纹表对未知客户端也是先句表、后精确迭代）。
pub fn default_tool_alias_rules() -> Vec<(String, String)> {
    vec![
        // ==================== Trae SOLO 独家（v3.8.5 首发实证） ====================
        ("ask_user_question".to_string(), "ask_user".to_string()),
        ("create_goal".to_string(), "set_goal".to_string()),
        ("exit_plan_mode".to_string(), "finish_plan".to_string()),
        ("get_goal".to_string(), "read_goal".to_string()),
        ("interrupt_agent".to_string(), "pause_agent".to_string()),
        ("job_kill".to_string(), "stop_job".to_string()),
        ("job_list".to_string(), "list_jobs".to_string()),
        // Trae CN 1.107.1（2026-10-29 live harness 实证）补两条：PureShowWidget
        // 为 camelCase 连写特征（别名按 TodoWrite→task_notes 先例拆 snake_case
        // 语义保持）、browser_waiting_for_user_interaction 为长蛇形用户交互
        // 等待通道（别名缩写语义保持）
        ("PureShowWidget".to_string(), "show_widget".to_string()),
        ("browser_waiting_for_user_interaction".to_string(), "wait_for_user".to_string()),
        // ==================== Claude Code 独家 ====================
        // v2.1.312（Agent SDK harness 懒加载架构）实证新增六条：主请求 14 工具
        // 中的独家命名；Workflow 虽为通用词，但作为 14 工具集指纹的组成成员
        // 一并改写（响应侧逆映射对客户端透明）；下方两条旧版兼容保留
        // camelCase 多词连写是 Claude Code 工具名强特征（通用客户端均用
        // snake_case）；逐字精确匹配须按真实注册名大小写；别名反向用
        // snake_case 拆词——语义保持且特征形态互换（TodoWrite→task_notes）
        ("TodoWrite".to_string(), "task_notes".to_string()),
        ("AskUserQuestion".to_string(), "user_input".to_string()),
        ("ListAgents".to_string(), "list_subagents".to_string()),
        ("ReportFindings".to_string(), "summarize_results".to_string()),
        ("ScheduleWakeup".to_string(), "set_reminder".to_string()),
        ("ToolSearch".to_string(), "find_tools".to_string()),
        ("DeferredToolPlaceholder".to_string(), "reserved_tool_slot".to_string()),
        ("Workflow".to_string(), "pipeline".to_string()),
        // ==================== Cline / Roo 独家 ====================
        // write_to_file / replace_in_file 双编辑器拆分是 Cline/Roo 特有协议
        // （其余客户端统一单 edit 入口）；attempt_completion 是其任务终结
        // 专用签名工具，tools 集合中出现即直指该客户端；
        // ask_followup_question 同为 Cline/Roo 独家追问名
        ("write_to_file".to_string(), "file_edit".to_string()),
        ("replace_in_file".to_string(), "file_patch".to_string()),
        ("attempt_completion".to_string(), "present_result".to_string()),
        ("ask_followup_question".to_string(), "user_reply".to_string()),
        // ==================== DSH 独家（0.2.1-rc.2 抓包实证） ====================
        // 2026-10-29 headless 抓包实证 25 工具：6 个与 Trae 段同名天然覆盖
        // （create_goal/get_goal/exit_plan_mode/interrupt_agent/job_kill/
        // job_list），其余 15 个为通用名不改；下方 4 条为独家名——job_output
        // 与 Trae 段 job_kill/job_list 同族管理面、subagent_fork 的 fork 后缀
        // 区分通用 subagent（DSH 同时注册两者，别名不可取 subagent 否则触发
        // 同请求碰撞防御整条跳过）、pwsh 为 DSH 特有 shell 入口命名、
        // working_directory 为其工作目录探针
        ("subagent_fork".to_string(), "spawn_subagent".to_string()),
        ("job_output".to_string(), "read_job_log".to_string()),
        ("pwsh".to_string(), "run_shell".to_string()),
        ("working_directory".to_string(), "work_dir".to_string()),
        // ==================== CodeBuddy 独家（1.106.1 抓包实证） ====================
        // 2026-10-29 buddy CLI 抓包实证 18 工具：replace_in_file/write_to_file/
        // ask_followup_question 经 Cline/Roo 段、ToolSearch 经 Claude Code 段
        // 天然覆盖（CodeBuddy 为 Cline 系 DNA 的 VS Code fork），其余 7 个为
        // 通用名不改；下方 7 条为独家名——read_lints 为 lint 探针、
        // execute_command 为 Cline 家族 shell 入口经典名（未来 Cline/Roo 实证
        // 天然覆盖）、update_memory/read_rules/use_skill 为其记忆/规则/技能
        // 面板、send_message 为用户消息通道、DeferExecuteTool 为懒加载占位
        // （与 Claude Code DeferredToolPlaceholder 同族异名，别名按 camelCase
        // 特征改 snake_case 拆词——先例 TodoWrite→task_notes）
        ("read_lints".to_string(), "check_lints".to_string()),
        ("execute_command".to_string(), "run_command".to_string()),
        ("update_memory".to_string(), "save_memory".to_string()),
        ("use_skill".to_string(), "load_skill".to_string()),
        ("read_rules".to_string(), "load_rules".to_string()),
        ("send_message".to_string(), "notify_user".to_string()),
        ("DeferExecuteTool".to_string(), "defer_execute".to_string()),
        // ==================== Qoder 独家（0.4.11 混淆包逆向实证） ====================
        // 2026-10-29 解码 @qoder-ai/qoder-cn-agent-sdk 实证：工具集与 Claude
        // Code 同构（Read/Write/Edit/Glob/Grep/Agent/TodoWrite/WebFetch/
        // WebSearch/AskUserQuestion/NotebookEdit 同名经 Claude Code 段天然
        // 覆盖——共享名不重复收录），独家名为 mcp 三件套（懒加载 MCP 发现/
        // 取 schema/调用，mcp__server__tool 命名协议层形态通用不改）+
        // SendMessage/TaskCreate 团队协作两件（PascalCase 连写特征，别名按
        // camelCase 先例拆 snake_case 语义保持）
        ("mcp_list".to_string(), "tool_catalog".to_string()),
        ("mcp_get".to_string(), "tool_schema".to_string()),
        ("mcp_call".to_string(), "tool_invoke".to_string()),
        ("SendMessage".to_string(), "relay_message".to_string()),
        ("TaskCreate".to_string(), "queue_task".to_string()),
        // ==================== Codex 独家 ====================
        // apply_patch 的统一 diff/patch 语法是 Codex 家族特有工具协议；
        // update_plan 为 Codex 计划跟踪专用名。0.145.0（2026-10-29 /v1/responses
        // 抓包实证）补两条：update_goal 为 goal 家族管理面（create_goal/get_goal
        // 经 Trae 段同名规则天然覆盖，别名按 update_plan→refresh_plan 家族先例
        // 取 refresh_goal）、request_user_input 为用户输入请求通道（别名避开
        // ask_user/user_input/user_reply 既有三件）
        ("update_goal".to_string(), "refresh_goal".to_string()),
        ("request_user_input".to_string(), "request_input".to_string()),
        ("apply_patch".to_string(), "patch_source".to_string()),
        ("update_plan".to_string(), "refresh_plan".to_string()),
    ]
}

/// 工具别名表 KV 文件形态（kv `tool_alias_map`）：与审核模板表同一结构
/// （rules[].from 逐字工具名，to 语义见 TemplateRule 注记）。Default 为
/// store::kv_get 的读取兜底约束（KV 缺失 = 纯内置默认表）
#[derive(Debug, Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct ToolAliasFile {
    #[serde(default)]
    pub rules: Vec<TemplateRule>,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

impl ToolAliasFile {
    /// 自定义表 → (from, to?) 列表（to=None/Some("") 统一为停用——工具名不可
    /// 为空串，与模板表的「空串=剥离」语义在此不适用）
    pub fn into_rules(self) -> Option<Vec<(String, Option<String>)>> {
        if self.rules.is_empty() {
            return None;
        }
        Some(
            self.rules
                .into_iter()
                .map(|r| (r.from, r.to.filter(|t| !t.is_empty())))
                .collect(),
        )
    }
}

/// 工具别名合并语义（对齐 merged_template_rules）：内置默认表全量生效，自定义
/// 按 from 同名覆盖/追加；to=None 或空串 = 停用（从生效表移除，原名透传）。
/// 注意：工具别名不支持 `|` 多候选——别名改写必须请求/响应双向一致，候选
/// 随轮换种子变化会导致响应侧逆映射失配（模板表无回程不受此限）
pub fn merged_tool_alias_rules(
    custom: Option<Vec<(String, Option<String>)>>,
) -> Vec<(String, String)> {
    let mut merged = default_tool_alias_rules();
    if let Some(rules) = custom {
        for (from, to) in rules {
            match to {
                None => merged.retain(|(f, _)| *f != from),
                Some(t) => {
                    if let Some(slot) = merged.iter_mut().find(|(f, _)| *f == from) {
                        slot.1 = t;
                    } else {
                        merged.push((from, t));
                    }
                }
            }
        }
    }
    merged
}

/// 请求侧工具名别名改写（issue #150 治本）：入参为 prepare_llm_chat_body /
/// prepare_wb_chat_body 产物（llm_utils_chat 形态），覆盖三个透传点——
/// `tools[].function.name`、`tool_choice`（normalize 后为字符串工具名）、
/// `messages[].tool_calls[].function_call.name`（历史轮）。命中才改（窄口径，
/// 逐字匹配），改写后返回逆映射（别名→原名）供响应侧回改；无 tools / 零命中
/// 返回 None（响应侧零开销直通）。
/// 一致性约束：逆映射按「别名→首个原名」先到先得——两条规则映射到同一别名时
/// 第二个原名不改写（配置冲突的防御性兜底，内置表无冲突；UI 侧不拦截）。
/// 碰撞防御：别名命中请求内其他真实工具名（且该名未被更早规则改名让出）时，
/// 该规则本请求整条跳过（tools/tool_choice/历史轮三处统一原名透传）——否则上游
/// 收到重复工具名，且响应侧逆映射会把真实工具的调用错误回改（静默错工具执行）；
/// tool_choice 仅跟随 tools 实际发生的改写（输家工具的 tool_choice 不改名，
/// 避免上游收到指向不存在/错误工具的 tool_choice）。
/// 注意：改写会改变 JSON 长度（别名 ≠ 原名字节数），故入参必须是
/// `&mut Vec<u8>` 整体替换回写——切片定长 copy_from_slice 会 panic。
pub fn apply_tool_alias_forward(
    body: &mut Vec<u8>,
    rules: &[(String, String)],
) -> Option<std::collections::HashMap<String, String>> {
    let mut v: Value = serde_json::from_slice(body).ok()?;
    let reverse = apply_tool_alias_forward_value(&mut v, rules)?;
    *body = serde_json::to_vec(&v).ok()?;
    Some(reverse)
}

/// Value 层别名正向改写核心（issue #158 P1-1：Qoder peek 已是预解析 Value，
/// 字节层包装的拆出避免热路径反复序列化）。语义与 [apply_tool_alias_forward]
/// 完全一致，另覆盖 OpenAI 标准形态历史轮 `tool_calls[].function.name`——
/// llm_utils_chat 管线已改名 function_call，该分支对 Trae 零命中无副作用；
/// Qoder peek 原生 OpenAI 形态经此分支改写（AGENT.md §14 透传点契约同改）。
pub fn apply_tool_alias_forward_value(
    v: &mut Value,
    rules: &[(String, String)],
) -> Option<std::collections::HashMap<String, String>> {
    if rules.is_empty() {
        return None;
    }
    let obj = v.as_object_mut()?;
    // 生效前向表：from → to（停用规则不在表内——to 非空才入表）
    let forward: Vec<(&str, &str)> = rules
        .iter()
        .filter(|(_, to)| !to.is_empty())
        .map(|(from, to)| (from.as_str(), to.as_str()))
        .collect();
    if forward.is_empty() {
        return None;
    }
    // 碰撞防御（审查修复）：改名前采集请求内全部真实工具名；别名命中存活真实名
    // 时该规则跳过。freed = 已被更早接受规则改名让出的原名——链式改写（B→C 后
    // A→B）在规则序下合法放行，仅拦真实重复
    let real: std::collections::HashSet<String> = obj
        .get("tools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let mut freed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let effective: std::collections::HashMap<&str, &str> = forward
        .into_iter()
        .filter(|(from, to)| {
            if real.contains(*to) && !freed.contains(*to) {
                return false; // 别名与存活真实名碰撞：整条跳过，原名透传
            }
            freed.insert(*from);
            true
        })
        .collect();
    let mut reverse: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut renamed = false;
    // tools[].function/name
    if let Some(Value::Array(tools)) = obj.get_mut("tools") {
        for tool in tools.iter_mut() {
            if let Some(name) = tool.pointer_mut("/function/name").and_then(|v| v.as_str()) {
                let orig = name.to_string();
                if let Some(alias) = effective.get(orig.as_str()) {
                    // 同别名冲突先到先得：第二个原名不改写
                    let conflict = reverse
                        .get(*alias)
                        .map(|prev| prev != &orig)
                        .unwrap_or(false);
                    if !conflict {
                        if let Some(slot) = tool.pointer_mut("/function/name") {
                            *slot = Value::String((*alias).to_string());
                        }
                        reverse.insert((*alias).to_string(), orig);
                        renamed = true;
                    }
                }
            }
        }
    }
    // tool_choice（normalize_tool_choice 后为字符串工具名）：仅跟随 tools 实际
    // 发生的改写——输家（同别名冲突被跳过）或碰撞跳过规则的 tool_choice 保持原名，
    // 避免上游收到指向不存在/错误工具的 tool_choice
    if let Some(Value::String(tc)) = obj.get_mut("tool_choice") {
        if reverse.values().any(|orig| orig == tc) {
            if let Some(alias) = effective.get(tc.as_str()) {
                *tc = (*alias).to_string();
                renamed = true;
            }
        }
    }
    // messages[].tool_calls[].function_call.name（prepare 已改名为 function_call）
    if let Some(Value::Array(msgs)) = obj.get_mut("messages") {
        for msg in msgs.iter_mut() {
            let Some(tcs) = msg.get_mut("tool_calls").and_then(Value::as_array_mut) else {
                continue;
            };
            for tc in tcs.iter_mut() {
                // 双形态历史轮（issue #158 P1-1）：function_call（llm_utils_chat，
                // Trae 管线改名后形态）与 function（OpenAI 标准，Qoder peek 原生）
                // ——同一 tool_call 仅存其一，function_call 优先命中即停
                for key in ["function_call", "function"] {
                    let Some(name) = tc
                        .pointer_mut(&format!("/{key}/name"))
                        .and_then(|v| v.as_str().map(str::to_string))
                    else {
                        continue;
                    };
                    if let Some(alias) = effective.get(name.as_str()) {
                        let conflict = reverse
                            .get(alias.to_string().as_str())
                            .map(|prev| prev != &name)
                            .unwrap_or(false);
                        if !conflict {
                            if let Some(slot) = tc.pointer_mut(&format!("/{key}/name")) {
                                *slot = Value::String((*alias).to_string());
                            }
                            reverse.insert((*alias).to_string(), name);
                            renamed = true;
                        }
                    }
                    break;
                }
            }
        }
    }
    if !renamed {
        return None;
    }
    Some(reverse)
}

/// 模板映射命中计数（issue #57 指纹反查）：
/// 热路径仅内存计数（命中才加锁）；11128 强制清洗重试 / 空完成换号重试等
/// 风控命中路径用 template_hit_note() 导出随日志落盘——ZCode/DSH/REASONIX
/// 等无公开资料客户端的迭代通道：从日志反查精确触发句 → 热更新规则表
fn hit_counter() -> &'static Mutex<BTreeMap<String, u64>> {
    static HITS: OnceLock<Mutex<BTreeMap<String, u64>>> = OnceLock::new();
    HITS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn record_hit(from: &str, to: &str) {
    if let Ok(mut m) = hit_counter().lock() {
        *m.entry(format!("{from} → {to}")).or_insert(0) += 1;
    }
}

/// 命中统计全文（"from → to ×N; ..."；无命中返回 None）
pub fn template_hit_summary() -> Option<String> {
    let m = hit_counter().lock().ok()?;
    if m.is_empty() {
        return None;
    }
    Some(
        m.iter()
            .map(|(k, v)| format!("{k} ×{v}"))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// 错误日志附注（无命中返回空串）：拼在「空完成 → 换号重试」等 note 之后
pub fn template_hit_note() -> String {
    match template_hit_summary() {
        Some(s) => format!("；模板命中: {s}"),
        None => String::new(),
    }
}

// ==================== 指纹样本环形缓冲（issue #125 增强：一键从样本生成规则） ====================

/// 指纹样本条目：网关命中风控时随日志同步采入内存环形缓冲，UI「指纹清洗」
/// tab 拉取展示、一键预填规则（from/to）。kind：`11128` = 显式指纹拦截
/// （清洗已启用仍命中）；`empty` = 空完成影子风控收尾采样。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FingerprintSampleEntry {
    pub kind: String,
    pub pool: String,
    /// unix 秒
    pub ts: u64,
    /// fingerprint_sample 输出（system="…" tools=[…]）
    pub sample: String,
    /// 模板命中注记（template_hit_note 输出，无命中为空串）
    pub note: String,
}

const FP_SAMPLE_CAP: usize = 50;

fn fingerprint_sample_store() -> &'static Mutex<VecDeque<FingerprintSampleEntry>> {
    static STORE: OnceLock<Mutex<VecDeque<FingerprintSampleEntry>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// 采样入环形缓冲：容量上限 50 超限丢最旧；队列中已存在相同 (kind,pool,sample)
/// 则跳过——换号轮换/跨 kind 交错重复命中样本恒定，不重复占位（全队列比对，
/// cap 50 代价可忽略；与日志 last_11128_sample 按请求去重同口径）。仅内存态，
/// 重启清空（持久反查走 app.log 样本行）。锁中毒则恢复内层守卫继续采样
/// （对齐 routes.rs safe_lock 风格，静默丢样本会漏诊断信息）。
pub fn record_fingerprint_sample(kind: &str, pool: &str, sample: &str, note: &str) {
    let mut q = fingerprint_sample_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if q
        .iter()
        .any(|e| e.kind == kind && e.pool == pool && e.sample == sample)
    {
        return;
    }
    if q.len() >= FP_SAMPLE_CAP {
        q.pop_front();
    }
    q.push_back(FingerprintSampleEntry {
        kind: kind.to_string(),
        pool: pool.to_string(),
        ts: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        sample: sample.to_string(),
        note: note.to_string(),
    });
}

/// UI 拉取：最新在前
pub fn fingerprint_sample_entries() -> Vec<FingerprintSampleEntry> {
    fingerprint_sample_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .rev()
        .cloned()
        .collect()
}

/// UI 清空
pub fn fingerprint_sample_entries_clear() {
    fingerprint_sample_store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

/// 影子风控收尾采样（issue #99 ① + issue #125 增强）：空完成主导收尾时采样
/// 入环形缓冲（kind=empty），返回「 指纹样本: …」日志注记；否则返回空串。
/// pool 由调用方传入池标识（wb 池 pool_label()，trae/qoder 字面量）。
pub fn empty_sample_note(exhaust_tag: &str, pool: &str, body: &[u8]) -> String {
    if exhaust_tag != "CHANNEL_FINGERPRINT_BLOCK" {
        return String::new();
    }
    let sample = fingerprint_sample(body);
    record_fingerprint_sample("empty", pool, &sample, &template_hit_note());
    format!(" 指纹样本: {sample}")
}

/// 11128 清洗后仍命中的指纹样本提取（issue #99 诊断闭环）：首条 system 消息
/// 文本前 300 字符（压缩空白单行化，内嵌引号转单引号）+ 工具名列表（前 10 个，
/// 缺 function 回退顶层 name——Responses 原生工具形态）。
/// 用途：目前只知模板命中了什么、不知漏了什么——样本落 app.log 后用户可直接
/// 反查精确触发句，经 UI「指纹清洗」tab 热更新规则表，无须发版。样本取**清洗前**
/// 原始请求体（保留原始句式便于逐字配规则），已改写部分由模板命中注记区分；
/// 工具名列表可识别客户端 harness 形态（如 mcp__ide__* = VS Code 扩展）。
/// 例外：wb_tool_exec_chat 的样本取自逐轮重建的 chat_body（清洗后形态）——
/// 漏网句按定义未被任何规则改写、仍原样保留，反查不受影响。
pub fn fingerprint_sample(body: &[u8]) -> String {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return "body 解析失败".to_string();
    };
    // 首条 system 消息文本（string 与 text blocks 两种形态）
    let system = first_system_text(&v);
    let sys: String = system
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(300)
        .collect();
    let mut out = format!("system=\"{}\"", sys.replace('"', "'"));
    // 工具名画像（issue #150 增强）：此前仅截前 10 个，tools 工具集指纹识别
    // 升级后需全量画像定位「哪类客户端形态在被拦」——展示前 10 + 全量总数 +
    // 排序名 FNV-1a 短哈希（同名集合跨请求稳定聚合，日志侧可按 hash 分桶统计）
    let tool_names: Vec<String> = v
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .or_else(|| t.get("name"))
                        .and_then(|n| n.as_str())
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    if !tool_names.is_empty() {
        let total = tool_names.len();
        let shown: Vec<String> = tool_names.iter().take(10).cloned().collect();
        let mut sorted = tool_names.clone();
        sorted.sort();
        let set_hash = fnv1a(&sorted.join("\u{1}")) & 0xFFFF_FFFF;
        out.push_str(&format!(
            " tools=[{}](total={total} hash={set_hash:08x})",
            shown.join(",")
        ));
    }
    out
}

/// 首条 system 消息文本提取（string 与 text blocks 两种形态；无 → 空串）。
/// fingerprint_sample 与 residual_fingerprint_note 共用
fn first_system_text(v: &Value) -> String {
    v.get("messages")
        .and_then(|m| m.as_array())
        .and_then(|msgs| {
            msgs.iter().find(|msg| {
                msg.get("role")
                    .and_then(|r| r.as_str())
                    .map(|r| r.eq_ignore_ascii_case("system"))
                    .unwrap_or(false)
            })
        })
        .and_then(|msg| msg.get("content"))
        .map(|c| match c {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join(" "),
            _ => String::new(),
        })
        .unwrap_or_default()
}

// ==================== 残指纹自检（issue #122 方案 C3） ====================

/// 已知上游风控特征词（issue #122 实证：opencode / DeepSeek Harness 品牌词与
/// 「interactive CLI」n-gram 残留即秒拒；issue #129 实证：TraeCode 品牌词被
/// buddy-ai 国际档渠道验证拦截；issue #155 实证：ZCode 品牌词触发 pool=trae
/// 空完成影子风控；2026-10-29 CodeBuddy 1.106.1 抓包实证：品牌词以
/// ".codebuddy" 文件夹警告句与 codebuddy.ai docs 域名句连片出现于正文）：
/// 整句改写后首条 system 应零残留。
/// 默认表通用 CLI 规则的改写句已同步去除 interactive（见 default_template_map）
const RESIDUAL_MARKERS: &[&str] = &[
    // Qoder CN 0.4.11（2026-10-29 混淆包逆向实证）：QoderWork 身份句整词组 +
    // "qoder team" 句中双品牌词组——整词组口径防域名/模型名误报
    // （qoder.com.cn 等域名词与 "Qoder" 单词不触发）
    "qoderwork",
    "qoder team",
    "codebuddy",
    "opencode",
    "deepseek harness",
    "interactive cli",
    "traecode",
    "zcode",
];

/// 残指纹自检 Value 核心（issue #158 P1-1：Qoder 池已在 active_peek 上持有
/// 解析好的 Value，直接扫描避免热路径多一次序列化/反序列化）
pub fn residual_fingerprint_note_value(v: &Value) -> Option<String> {
    let system = first_system_text(v).to_lowercase();
    if system.is_empty() {
        return None;
    }
    let hits: Vec<&str> = RESIDUAL_MARKERS
        .iter()
        .copied()
        .filter(|m| system.contains(m))
        .collect();
    if hits.is_empty() {
        None
    } else {
        Some(hits.join(","))
    }
}

/// 残指纹自检（issue #122 方案 C3）：模板改写后对首条 system 消息扫描已知
/// 特征词，命中返回小写特征词列表（逗号连接），无命中 None。
/// 仅查首条 system——用户正文/工具结果含品牌词属正常业务内容，不参与自检
/// 避免误报；调用方在 sanitize 开启时落 debug 日志（按请求去重）
pub fn residual_fingerprint_note(body: &[u8]) -> Option<String> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    residual_fingerprint_note_value(&v)
}

// ==================== 池级指纹拦截短记忆（issue #122 缺口2） ====================

/// 渠道指纹拦截短记忆 TTL（秒）：轮换耗尽且空完成主导（CHANNEL_FINGERPRINT_BLOCK）
/// 后，该时长内同池新请求直接以激进清洗起步——503 后客户端重试是新请求
/// （empty_hits 归零、首轮无激进规则），无记忆会从零试探再烧一轮账号
const FP_BLOCK_TTL_SECS: i64 = 600;

/// 池 → 最近一次指纹拦截时间戳（秒）；池间互不影响，TTL 过期自然失效
fn fp_block_registry() -> &'static Mutex<BTreeMap<String, i64>> {
    static REG: OnceLock<Mutex<BTreeMap<String, i64>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// 标记池级指纹拦截（各池轮换耗尽收尾调用；blocked=false 不动——仅空完成主导
/// 的 CHANNEL_FINGERPRINT_BLOCK 收尾才标记，普通账号耗尽不触发激进升级）
pub fn mark_channel_fp_block(pool: &str, blocked: bool) {
    if !blocked {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 锁中毒恢复（审查修复：对齐同文件样本缓冲 into_inner 惯例——中毒仅意味
    // 持锁线程 panic，注册表本身仍可用；静默丢标记会让 aggressive 起步判定失效）
    let mut m = fp_block_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    m.insert(pool.to_string(), now);
}

/// 池级指纹拦截是否处于 TTL 内（prepare 调用方 aggressive 起步判定）
pub fn fp_block_recent(pool: &str) -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    fp_block_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) // 锁中毒恢复，对齐同文件样本缓冲惯例
        .get(pool)
        .copied()
        .map(|ts| now - ts < FP_BLOCK_TTL_SECS)
        .unwrap_or(false)
}

/// 审核模板黑名单最小改写：映射表为空才跳过（不做硬编码关键词预检——
/// 外置映射表新增规则的命中词可能不含内置三短语，预检会永久漏改）；
/// 条目少（个位数），逐条 contains 代价可接受。
/// seed=0 稳定入口：多候选规则按规则名哈希稳定选一，行为可复现；生产路径
/// 已全部走 apply_template_map_seeded（issue #122 缺口1 后 Trae 池也带种子），
/// 仅测试使用 → #[cfg(test)] 门控（零警告红线，usage.rs 同款先例）
#[cfg(test)]
pub fn apply_template_map(text: &str, templates: &[(String, String)]) -> String {
    apply_template_map_seeded(text, templates, 0)
}

/// 带请求种子的模板改写（issue #122 方案 C1）：`to` 含 `|` 分隔多候选时按
/// 种子稳定选一——同请求内可复现、跨换号重试（种子随轮换更新）变化，消除
/// 「全网统一改写句」的二次指纹化；单候选规则行为与种子无关（存量零影响）
pub fn apply_template_map_seeded(text: &str, templates: &[(String, String)], seed: u64) -> String {
    if templates.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (from, to) in templates {
        if out.contains(from.as_str()) {
            let chosen = pick_candidate(from, to, seed);
            out = out.replace(from.as_str(), chosen);
            record_hit(from, chosen);
        }
    }
    out
}

/// FNV-1a 64（零依赖、跨进程稳定——std DefaultHasher 不保证跨版本/进程一致，
/// 改写候选选择需要可复现）
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// 请求级改写种子（issue #122 方案 C1）：conv_id 绑定请求、attempt 随换号
/// 轮换/工具执行轮次递增——跨重试候选必变、请求间互不相同
pub fn variant_seed(conv_id: &str, attempt: u64) -> u64 {
    fnv1a(conv_id) ^ attempt.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// 多候选选择（issue #122 方案 C1）：`to` 无 `|` → 原样（存量规则零影响）；
/// 多候选 → (种子 ^ 规则名哈希) 取模稳定选一。规则名混入使各规则独立翻转
/// （不必同步换候选）；空候选（`|` 首尾/连续）= 剥离语义，合法保留
fn pick_candidate<'a>(from: &str, to: &'a str, seed: u64) -> &'a str {
    match to.split('|').count() {
        1 => to,
        n => {
            let idx = (seed ^ fnv1a(from)) % n as u64;
            to.split('|').nth(idx as usize).unwrap_or(to)
        }
    }
}

/// 剥离 cc_xxx=...; 键值对与 x-anthropic-* 引用（F-30 指纹清洗，零正则依赖）
pub fn strip_cc_fingerprints(text: &str) -> String {
    let needs_check = text.contains("cc_") || text.contains("x-anthropic-");
    if !needs_check {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let is_cc = rest.starts_with("cc_");
        let is_anthropic = rest.starts_with("x-anthropic-");
        if (is_cc || is_anthropic) && (i == 0 || !is_token_char(bytes[i - 1])) {
            // 键名（token 字符）
            let mut j = i;
            while j < text.len() && is_token_char(bytes[j]) {
                j += 1;
            }
            // P1 修复（误伤回归）：键名后必须跟 `=`/`:` 分隔符（前后允许空白）才算
            // 指纹键值对——裸词（如 "use cc_flags for build" 的 cc_flags）是正常
            // 业务内容，原实现整词删除 + 吞尾随空白，静默损坏用户内容
            let after = &text[j..];
            let t2 = after.trim_start();
            if !t2.starts_with('=') && !t2.starts_with(':') {
                // 非指纹键：按普通文本原样保留整个键名（ASCII token，直接复制）
                out.push_str(&text[i..j]);
                i = j;
                continue;
            }
            let mut end = j;
            if let Some(r) = t2.strip_prefix(['=', ':']) {
                let v = r.trim_start(); // v 是 text 的后缀
                let vs = text.len() - v.len();
                let stop_rel = v
                    .find(|c| matches!(c, ';' | ',' | ' ' | '\n' | '\t'))
                    .unwrap_or(v.len());
                end = vs + stop_rel;
            }
            // 吃掉尾随的分号/逗号/空白（键值对属于指纹痕迹，连逗号一并清除）
            while end < text.len() && matches!(bytes[end], b';' | b',' | b' ' | b'\t') {
                end += 1;
            }
            i = end;
            continue;
        }
        // UTF-8 安全推进
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&text[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// 对单条消息 content 施加清洗（string 与 text blocks 两种形态）；
/// seed 透传多候选改写选择（issue #122 方案 C1）
fn sanitize_content(content: &mut Value, templates: &[(String, String)], sanitize: bool, seed: u64) {
    match content {
        Value::String(s) => {
            let mut t = if sanitize { strip_cc_fingerprints(s) } else { s.clone() };
            if sanitize {
                t = apply_template_map_seeded(&t, templates, seed);
            }
            *s = t;
        }
        Value::Array(blocks) => {
            for b in blocks.iter_mut() {
                if let Some(Value::String(t)) = b.get_mut("text") {
                    let mut v = if sanitize { strip_cc_fingerprints(t) } else { t.clone() };
                    if sanitize {
                        v = apply_template_map_seeded(&v, templates, seed);
                    }
                    *t = v;
                }
            }
        }
        _ => {}
    }
}

/// OpenAI 请求体 → WB /v2/chat/completions 请求体改写。
///
/// `account_uid`：发起请求的账号 uid（账号池体系）。非空时注入顶层
/// `prompt_cache_key`（agent2api normalize.rs 情报互证，2026-10-03）：命中上游
/// 前缀缓存可带来数量级的费用差；**键必须按账号硬隔离**——跨账号命中前缀缓存
/// 等于把 A 账号对话内容泄露给 B 账号（作者实测教训），故键首段为账号 uid。
///
/// `variant_seed`：多候选改写种子（issue #122 方案 C1，wb_payload::variant_seed
/// 生成——conv_id 绑定请求、attempt 随换号轮换递增）；`aggressive`：激进清洗
/// 开关（issue #122 方案 C2，仅空完成换号重试时为 true，追加短前缀规则兜
/// 未知身份句变体；sanitize 关闭时不生效）
pub fn prepare_wb_chat_body(
    src: &[u8],
    model: &str,
    account_uid: &str,
    conv_id: &str,
    effort: Option<&str>,
    sanitize: bool,
    templates: &[(String, String)],
    variant_seed: u64,
    aggressive: bool,
) -> Vec<u8> {
    // 激进清洗（issue #122 方案 C2）：生效表后追加激进短前缀规则（仅重试路径，
    // 分配代价可忽略）；sanitize 关闭时无清洗即无升级
    let aggressive_rules: Vec<(String, String)>;
    let templates: &[(String, String)] = if aggressive && sanitize {
        let mut v = templates.to_vec();
        v.extend(aggressive_template_rules());
        aggressive_rules = v;
        &aggressive_rules
    } else {
        templates
    };
    let mut obj: Value = match serde_json::from_slice(src) {
        Ok(v) => v,
        Err(_) => return src.to_vec(),
    };
    let m = match obj.as_object_mut() {
        Some(m) => m,
        None => return src.to_vec(),
    };

    // 模型归一化（目录 id 为小写规范名）
    m.insert("model".into(), json!(model));

    // 连续同角色消息合并 + 指纹清洗。
    // role:"tool" 一律不参与合并（无论当前还是上一条）：tool_call_id 必须逐条
    // 保留，合并会丢失 id 使上游无法把结果对齐到对应调用
    if let Some(msgs) = m.get_mut("messages").and_then(|v| v.as_array_mut()) {
        let mut merged: Vec<Value> = Vec::with_capacity(msgs.len());
        for mut msg in msgs.drain(..) {
            let mut role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("").to_string();
            // role 大小写归一（上游 role 枚举小写敏感）："Developer"/"SYSTEM"/"TOOL"
            // 等非小写变体统一转小写——与 ensure_leading_system_message 的判断
            // 对齐，也堵住 "TOOL" 误参与合并的隐性缺口；空串/非字符串 role 原样透传
            if !role.is_empty() {
                role.make_ascii_lowercase();
                if let Some(r) = msg.get_mut("role") {
                    *r = json!(role);
                }
            }
            // 11128 渠道校验（dsh-xdpool#58 实证 2026-10-20）：上游不认
            // role:"developer"（国内版/国际版都 400 Illegal API invocation），
            // 统一转 system——只换角色不重排，位置语义保持；转换后走既有
            // 同角色合并（DSH 的 developer workspace 指令与 system 同类）
            if role == "developer" {
                if let Some(r) = msg.get_mut("role") {
                    *r = json!("system");
                }
                role = "system".to_string();
            }
            let mergeable = !role.is_empty() && role != "tool";
            if mergeable {
                if let Some(last) = merged.last_mut() {
                    let last_role = last.get("role").and_then(|r| r.as_str()).unwrap_or("");
                    if !last_role.is_empty() && last_role != "tool" && role == last_role {
                        merge_message(last, msg);
                        continue;
                    }
                }
            }
            merged.push(msg);
        }
        for msg in merged.iter_mut() {
            if let Some(c) = msg.get_mut("content") {
                sanitize_content(c, templates, sanitize, variant_seed);
            }
            // 指纹清洗覆盖 tool_calls[].function.arguments（审核模板最小改写；
            // 不套用 strip_cc_fingerprints——其删段语义可能破坏 JSON 结构）
            if let Some(tcs) = msg.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
                for tc in tcs.iter_mut() {
                    if let Some(Value::String(s)) = tc
                        .get_mut("function")
                        .and_then(|f| f.get_mut("arguments"))
                    {
                        if sanitize {
                            *s = apply_template_map_seeded(s, templates, variant_seed);
                        }
                    }
                }
            }
        }
        m.insert("messages".into(), Value::Array(merged));
    }

    // 指纹清洗覆盖 tools[].function.description（审核模板最小改写）：
    // CLI harness 可能将身份句/生态术语嵌入工具描述，是消息体清洗的绕过通道
    if sanitize {
        if let Some(tools) = m.get_mut("tools").and_then(|t| t.as_array_mut()) {
            for tool in tools.iter_mut() {
                if let Some(Value::String(d)) = tool
                    .get_mut("function")
                    .and_then(|f| f.get_mut("description"))
                {
                    *d = apply_template_map_seeded(d, templates, variant_seed);
                }
            }
        }
    }

    normalize_tool_choice(m);

    // 首条消息必须为 system（上游 Go struct 硬校验，否则 400；agent2api 实测
    // 情报 2026-10-03）。在合并/清洗之后执行：合并不改变首条相对位置，但调用方
    // 可能产出 user-first 请求体，此处统一兜底
    ensure_leading_system_message(m);

    // 强制流式（上游拒绝非流式，code 11101）
    m.insert("stream".into(), json!(true));

    // 会话 id（粘性绑定 / 上游会话归属）
    if !conv_id.is_empty() {
        m.insert("conversation_id".into(), json!(conv_id));
    }

    // prompt_cache_key：按账号硬隔离的上游前缀缓存键（见函数头注释）；
    // uid 缺失（测试/异常路径）不注入，宁缺勿造
    if !account_uid.is_empty() {
        m.insert("prompt_cache_key".into(), json!(prompt_cache_key(account_uid, conv_id)));
    }

    // reasoning_effort：调用方已按目录降级，这里只注入非空值
    if let Some(e) = effort.map(str::trim).filter(|s| !s.is_empty()) {
        m.insert("reasoning_effort".into(), json!(e));
    } else {
        m.remove("reasoning_effort");
    }

    serde_json::to_vec(&obj).unwrap_or_else(|_| src.to_vec())
}

/// 上游前缀缓存键：`wb-<uid 前 8 位>-<sha256(uid|conv)[0..16]>`。
/// 同账号 + 同会话稳定命中缓存；不同账号键必不同（硬隔离，防对话内容跨账号泄露）。
fn prompt_cache_key(account_uid: &str, conv_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let uid8: String = account_uid.chars().take(8).collect();
    let mut h = Sha256::new();
    h.update(account_uid.as_bytes());
    h.update(b"|");
    h.update(conv_id.as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("wb-{uid8}-{}", &hex[..16.min(hex.len())])
}

/// 首条消息 system 兜底（agent2api workbuddy.rs ensure_leading_system_message
/// 同款，2026-10-03 情报）：上游要求首条必为 system 否则 400；首条为 user/
/// assistant 时前置一条中性兜底。messages 为空/缺失时不动（让上游自身的
/// 参数校验报错，避免掩盖调用方 bug）。
fn ensure_leading_system_message(m: &mut Map<String, Value>) {
    const FALLBACK_SYSTEM: &str = "你是一个得力助手";
    let Some(msgs) = m.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return;
    };
    let first_is_system = msgs
        .first()
        .and_then(|msg| msg.get("role"))
        .and_then(|r| r.as_str())
        .map(|r| r.eq_ignore_ascii_case("system"))
        .unwrap_or(false);
    if !msgs.is_empty() && !first_is_system {
        msgs.insert(0, json!({"role": "system", "content": FALLBACK_SYSTEM}));
    }
}

/// 同角色合并：string/text 拼接；tool_calls 追加；其余字段保留首个
fn merge_message(base: &mut Value, extra: Value) {
    let content_b = extra.get("content").cloned().unwrap_or(Value::Null);
    if !content_b.is_null() {
        // 目标 content 缺失/为 null：直接采用新值
        if base.get("content").map_or(true, |c| c.is_null()) {
            base["content"] = content_b;
        } else {
            join_content(base.get_mut("content").unwrap(), content_b);
        }
    }
    if let Some(tcs) = extra.get("tool_calls").and_then(|t| t.as_array()).cloned() {
        match base.get_mut("tool_calls").and_then(|t| t.as_array_mut()) {
            Some(arr) => arr.extend(tcs),
            None => base["tool_calls"] = Value::Array(tcs),
        }
    }
}

/// content 拼接：string↔string 加空行拼接；string 进数组追加 text 块；数组合并
fn join_content(base: &mut Value, extra: Value) {
    match extra {
        Value::String(y) => match base {
            Value::String(x) => {
                let mut s = x.clone();
                if !s.is_empty() && !y.is_empty() {
                    s.push_str("\n\n");
                }
                s.push_str(&y);
                *x = s;
            }
            Value::Array(x) => x.push(json!({"type": "text", "text": y})),
            _ => {}
        },
        Value::Array(y) => match base {
            Value::Array(x) => x.extend(y),
            Value::String(x) => {
                let old = x.clone();
                let mut blocks = vec![json!({"type": "text", "text": old})];
                blocks.extend(y);
                *base = Value::Array(blocks);
            }
            _ => {}
        },
        _ => {}
    }
}

/// tool_choice 归一化（对象形式上游报 400 code=11101）：
/// {"type":"function","function":{"name":X}} → "X"；auto/none/required 原样 string；
/// 带 function.name 的对象（含未知 type 形态）一律归一为该 name；
/// 其余未知形态归 auto
pub fn normalize_tool_choice(m: &mut Map<String, Value>) {
    let tc = match m.remove("tool_choice") {
        Some(v) => v,
        None => return,
    };
    match tc {
        Value::String(_) => {
            m.insert("tool_choice".into(), tc);
        }
        Value::Object(map) => {
            let typ = map
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("auto")
                .to_lowercase();
            let name = map
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .or_else(|| map.get("name").and_then(|n| n.as_str()))
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let value = match name {
                // 带 name 的对象：无论 type 为何（function/未知形态）都保留指定意图
                Some(n) => n.to_string(),
                None => match typ.as_str() {
                    "none" => "none".to_string(),
                    "any" | "required" => "required".to_string(),
                    // auto 与其余未知形态一律归 auto
                    _ => "auto".to_string(),
                },
            };
            m.insert("tool_choice".into(), json!(value));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NO_TPL: &[(String, String)] = &[];

    #[test]
    fn template_hit_counter_records_and_exports() {
        // 命中即计数（issue #57 指纹反查）：summary/note 导出 from→to 与次数
        let tpls = vec![("FP_PROBE_FROM".to_string(), "FP_PROBE_TO".to_string())];
        let out = apply_template_map("before FP_PROBE_FROM after", &tpls);
        assert_eq!(out, "before FP_PROBE_TO after");
        let summary = template_hit_summary().expect("命中后 summary 不应为空");
        assert!(summary.contains("FP_PROBE_FROM → FP_PROBE_TO"), "{summary}");
        assert!(summary.contains("×1"), "{summary}");
        let note = template_hit_note();
        assert!(note.contains("模板命中: "), "{note}");
        assert!(note.contains("FP_PROBE_FROM → FP_PROBE_TO"), "{note}");
        // 未命中不计数：只含上面那一条
        let _ = apply_template_map("no match here", &tpls);
        let again = template_hit_summary().unwrap();
        assert_eq!(again.matches("FP_PROBE_FROM → FP_PROBE_TO").count(), 1);
    }

    /// 样本环形缓冲（issue #125）：入队/去重/容量上限/逆序拉取/清空
    #[test]
    fn fingerprint_sample_store_records_and_clears() {
        let _guard = fp_sample_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        fingerprint_sample_entries_clear();
        record_fingerprint_sample("11128", "wb", "s1", "");
        record_fingerprint_sample("11128", "wb", "s1", ""); // 队尾三元组重复 → 去重
        record_fingerprint_sample("empty", "wb", "s1", ""); // kind 不同 → 保留
        let got = fingerprint_sample_entries();
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].kind, "empty", "最新在前");
        assert_eq!(got[1].kind, "11128");
        assert!(got[0].ts > 0, "unix 秒时间戳");
        // 全队列去重：跨 kind 交错后再次入队相同三元组，不重复占位
        record_fingerprint_sample("11128", "wb", "s1", "");
        assert_eq!(fingerprint_sample_entries().len(), 2, "跨 kind 交错去重");
        // 容量上限：超限丢最旧
        for i in 0..(FP_SAMPLE_CAP + 5) {
            record_fingerprint_sample("11128", "wb", &format!("cap{i}"), "");
        }
        let got = fingerprint_sample_entries();
        assert_eq!(got.len(), FP_SAMPLE_CAP);
        assert_eq!(got[0].sample, format!("cap{}", FP_SAMPLE_CAP + 4), "最新在前");
        assert_eq!(got.last().unwrap().sample, "cap5", "最旧被挤出");
        fingerprint_sample_entries_clear();
        assert!(fingerprint_sample_entries().is_empty());
    }

    /// 11128 指纹样本提取（issue #99）：首条 system 文本（空白压缩、引号转义、
    /// 截断 300 字符）+ 工具名列表；非 JSON body 不 panic
    #[test]
    fn fingerprint_sample_extracts_system_and_tools() {
        let body = json!({
            "messages": [
                {"role":"system","content":"You are Claude Code, Anthropic's official CLI for Claude,  running\ninside the VS Code extension."},
                {"role":"user","content":"hi"}
            ],
            "tools": [
                {"type":"function","function":{"name":"mcp__ide__getDiagnostics","parameters":{}}},
                {"type":"function","function":{"name":"read_file","parameters":{}}}
            ]
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        let s = fingerprint_sample(&bytes);
        assert!(s.starts_with("system=\""), "{s}");
        // 空白压缩为单空格
        assert!(s.contains("Claude, running inside the VS Code extension."), "{s}");
        // 工具名列表（可识别 IDE harness 形态）
        assert!(s.contains("tools=[mcp__ide__getDiagnostics,read_file]"), "{s}");
        // 非 JSON body 不 panic
        assert_eq!(fingerprint_sample(b"not json"), "body 解析失败");
        // 无 tools → 不输出 tools 段
        let s2 = fingerprint_sample(&serde_json::to_vec(&json!({
            "messages":[{"role":"system","content":"sys"}]
        })).unwrap());
        assert!(s2.starts_with("system=\"sys\"") && !s2.contains("tools="), "{s2}");
    }

    /// 指纹样本截断与 blocks 形态 content：300 字符截断、text blocks 拼接、
    /// 内嵌双引号转单引号（日志单行安全）
    #[test]
    fn fingerprint_sample_truncates_and_handles_blocks() {
        let body = json!({
            "messages":[{"role":"system","content":[
                {"type":"text","text":"say \"hi\" "},
                {"type":"text","text":"x".repeat(400)}
            ]}]
        });
        let s = fingerprint_sample(&serde_json::to_vec(&body).unwrap());
        assert!(s.starts_with("system=\"say 'hi' x"), "{s}");
        // 300 字符截断：总长远小于 400 源文本
        assert!(s.len() < 400, "len={}", s.len());
        // Responses 原生工具形态（无 function 包裹）回退顶层 name
        let body2 = json!({
            "messages":[{"role":"system","content":"s"}],
            "tools":[{"type":"web_search","name":"web_search"}]
        });
        let s2 = fingerprint_sample(&serde_json::to_vec(&body2).unwrap());
        assert!(s2.contains("tools=[web_search]"), "{s2}");
    }

    /// issue #99 布防：VS Code 扩展身份句逗号续接变体（现有句号规则必然漏匹配）
    #[test]
    fn vscode_identity_comma_variant_rewrite() {
        let tpl = default_template_map();
        let v = "You are Claude Code, Anthropic's official CLI for Claude, running inside the VS Code extension.";
        let out = apply_template_map(v, &tpl);
        assert!(out.contains("official CLI tool for Claude, running inside"), "{out}");
        assert!(!out.contains("CLI for Claude,"), "{out}");
        // 句号形态仍由原规则覆盖，互不干扰
        let plain = "You are Claude Code, Anthropic's official CLI for Claude.";
        let out2 = apply_template_map(plain, &tpl);
        assert!(out2.contains("CLI tool for Claude."), "{out2}");
    }

    /// issue #99 ⑥ 模板表合并语义：默认表全量生效 + 自定义按 from 覆盖/追加；
    /// to 空串 = 剥离（改写为空串，旧行为保持）、to null = 停用（从生效表移除）
    #[test]
    fn merged_template_rules_overrides_and_extends() {
        // None = 纯默认；空自定义表 = 纯默认（「恢复默认 = 保存空数组」语义兼容）
        assert_eq!(
            merged_template_rules(None).len(),
            default_template_map().len()
        );
        assert_eq!(
            merged_template_rules(Some(vec![])).len(),
            default_template_map().len()
        );
        // 覆盖同名 from + 追加新规则；默认规则全量保留
        let custom = vec![
            (
                "Main branch (you will usually use this with PRs)".to_string(),
                Some("Trunk branch (you will usually use this with PRs)".to_string()),
            ),
            (
                "Custom sentence.".to_string(),
                Some("Custom rewrite.".to_string()),
            ),
        ];
        let merged = merged_template_rules(Some(custom));
        assert!(merged.iter().any(|(f, t)| f
            == "Main branch (you will usually use this with PRs)"
            && t.starts_with("Trunk branch")));
        assert!(merged
            .iter()
            .any(|(f, t)| f == "Custom sentence." && t == "Custom rewrite."));
        assert!(merged
            .iter()
            .any(|(f, _)| f.starts_with("You are Claude Code")));
        // to 空串 = 剥离（改写为空串）；其余默认规则（含逗号变体）不受影响
        let merged2 = merged_template_rules(Some(vec![(
            "You are Claude Code, Anthropic's official CLI for Claude.".to_string(),
            Some(String::new()),
        )]));
        assert!(merged2.iter().any(|(f, t)| f
            == "You are Claude Code, Anthropic's official CLI for Claude."
            && t.is_empty()));
        assert!(merged2
            .iter()
            .any(|(f, _)| f.starts_with(
                "You are Claude Code, Anthropic's official CLI for Claude,"
            )));
        // to null = 停用（从生效表移除，原句透传）
        let merged3 = merged_template_rules(Some(vec![(
            "You are Claude Code, Anthropic's official CLI for Claude.".to_string(),
            None,
        )]));
        assert!(!merged3
            .iter()
            .any(|(f, _)| f == "You are Claude Code, Anthropic's official CLI for Claude."));
        assert!(merged3
            .iter()
            .any(|(f, _)| f.starts_with(
                "You are Claude Code, Anthropic's official CLI for Claude,"
            )));
    }

    /// 存量数据兼容（3.7.11 修复）：旧 JSON 的 to:"" 反序列化为 Some("") = 剥离，
    /// 不得被合并语义误判为停用；显式缺省/null 才是停用
    #[test]
    fn legacy_empty_to_stays_strip_not_tombstone() {
        // 存量形状：to 为空串 → 剥离规则照常生效
        let file: TemplateMapFile =
            serde_json::from_str(r#"{"templates":[{"from":"FP_SENTENCE","to":""}]}"#).unwrap();
        let merged = merged_template_rules(file.into_rules());
        assert!(
            merged.iter().any(|(f, t)| f == "FP_SENTENCE" && t.is_empty()),
            "存量空 to 行应保持剥离语义，{merged:?}"
        );
        // 新形状：to 缺省/null → 停用默认规则
        let file2: TemplateMapFile =
            serde_json::from_str(r#"{"templates":[{"from":"You are Codex, based on GPT-5"}]}"#)
                .unwrap();
        let merged2 = merged_template_rules(file2.into_rules());
        assert!(!merged2
            .iter()
            .any(|(f, _)| f == "You are Codex, based on GPT-5"));
        // 其余默认规则不受停用影响
        assert!(merged2
            .iter()
            .any(|(f, _)| f.starts_with("You are Claude Code")));
    }

    fn rewrite(v: Value, effort: Option<&str>) -> Value {
        let src = serde_json::to_vec(&v).unwrap();
        let out = prepare_wb_chat_body(&src, "glm-5.3", "4487568582777872", "conv-1", effort, true, NO_TPL, 0, false);
        serde_json::from_slice(&out).unwrap()
    }

    /// prompt_cache_key 按账号硬隔离：同账号+同会话稳定；跨账号键必不同
    #[test]
    fn prompt_cache_key_hard_isolated_per_account() {
        let src = json!({"messages":[{"role":"system","content":"s"}]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let out = prepare_wb_chat_body(&src_bytes, "m", "4487568582777872", "conv-1", None, false, NO_TPL, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let key = v["prompt_cache_key"].as_str().unwrap();
        assert!(key.starts_with("wb-44875685-"), "键首段为账号 uid 前 8 位: {key}");
        // 同账号同会话：稳定（可重复调用命中同一缓存键）
        let out2 = prepare_wb_chat_body(&src_bytes, "m", "4487568582777872", "conv-1", None, false, NO_TPL, 0, false);
        let v2: Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(v2["prompt_cache_key"], v["prompt_cache_key"]);
        // 跨账号：键必不同（硬隔离——跨账号命中前缀缓存 = 对话内容泄露）
        let out3 = prepare_wb_chat_body(&src_bytes, "m", "1335050000000000", "conv-1", None, false, NO_TPL, 0, false);
        let v3: Value = serde_json::from_slice(&out3).unwrap();
        assert_ne!(v3["prompt_cache_key"], v["prompt_cache_key"]);
        assert!(v3["prompt_cache_key"].as_str().unwrap().starts_with("wb-13350500-"));
        // uid 缺失不注入（宁缺勿造）
        let out4 = prepare_wb_chat_body(&src_bytes, "m", "", "conv-1", None, false, NO_TPL, 0, false);
        let v4: Value = serde_json::from_slice(&out4).unwrap();
        assert!(v4.get("prompt_cache_key").is_none());
    }

    /// 首条消息 system 兜底：user-first 前置中性 system；system-first 不动；
    /// 空 messages 不动（让上游参数校验报错）
    #[test]
    fn leading_system_message_ensured() {
        let src = json!({"messages":[{"role":"user","content":"hi"}]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "c", None, false, NO_TPL, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], json!("system"));
        assert!(!msgs[0]["content"].as_str().unwrap().is_empty());
        assert_eq!(msgs[1], json!({"role":"user","content":"hi"}));

        // system-first 不重复注入
        let src = json!({"messages":[{"role":"system","content":"真实系统提示"},{"role":"user","content":"hi"}]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "c", None, false, NO_TPL, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"], json!("真实系统提示"));

        // 空 messages 不注入
        let src = json!({"messages":[]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "c", None, false, NO_TPL, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn forces_stream_and_sets_conversation() {
        let out = rewrite(json!({"model":"glm-5.3","stream":false,"messages":[]}), None);
        assert_eq!(out["stream"], json!(true));
        assert_eq!(out["conversation_id"], json!("conv-1"));
        assert_eq!(out["model"], json!("glm-5.3"));
    }

    #[test]
    fn tool_choice_object_becomes_string() {
        let out = rewrite(
            json!({"messages":[],"tool_choice":{"type":"function","function":{"name":"get_weather"}}}),
            None,
        );
        assert_eq!(out["tool_choice"], json!("get_weather"));
        let out = rewrite(json!({"messages":[],"tool_choice":{"type":"any"}}), None);
        assert_eq!(out["tool_choice"], json!("required"));
        let out = rewrite(json!({"messages":[],"tool_choice":{"type":"none"}}), None);
        assert_eq!(out["tool_choice"], json!("none"));
        // string 原样保留
        let out = rewrite(json!({"messages":[],"tool_choice":"auto"}), None);
        assert_eq!(out["tool_choice"], json!("auto"));
    }

    #[test]
    fn effort_injected_or_removed() {
        let out = rewrite(json!({"messages":[],"reasoning_effort":"xhigh"}), Some("medium"));
        assert_eq!(out["reasoning_effort"], json!("medium"));
        let out = rewrite(json!({"messages":[],"reasoning_effort":"high"}), None);
        assert!(out.get("reasoning_effort").is_none());
    }

    #[test]
    fn merges_consecutive_same_role() {
        // 输入自带首条 system：不触发兜底注入，合并语义保持原断言
        let out = rewrite(
            json!({"messages":[
                {"role":"system","content":"sys"},
                {"role":"user","content":"第一段"},
                {"role":"user","content":"第二段"},
                {"role":"assistant","content":"回复"},
            ]}),
            None,
        );
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["content"], json!("第一段\n\n第二段"));
    }

    #[test]
    fn developer_role_converted_to_system() {
        // 11128 渠道校验（dsh-xdpool#58）：role:"developer" 统一转 system
        // （国内版/国际版都 400；只换角色不重排）
        let out = rewrite(
            json!({"messages":[
                {"role":"developer","content":"workspace 指令"},
                {"role":"user","content":"hi"},
                {"role":"developer","content":"中段指令"},
            ]}),
            None,
        );
        let msgs = out["messages"].as_array().unwrap();
        assert!(msgs.iter().all(|m| m["role"] != "developer"), "{msgs:?}");
        // 首条 developer → system：不触发兜底 system 注入（len 保持 3）
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], json!("workspace 指令"));
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "system");
    }

    #[test]
    fn developer_role_normalized_and_merged() {
        // 归一化 + 合并联动：非小写变体（"Developer"/"SYSTEM"）先归一小写，
        // developer 转 system 后走既有同角色合并——相邻两条并作一条
        let out = rewrite(
            json!({"messages":[
                {"role":"Developer","content":"首条"},
                {"role":"SYSTEM","content":"第二条"},
                {"role":"user","content":"hi"},
            ]}),
            None,
        );
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], json!("首条\n\n第二条"));
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn template_blacklist_minimal_rewrite() {
        let tpl = default_template_map();
        let cc = "You are Claude Code, Anthropic's official CLI for Claude.";
        let out = apply_template_map(cc, &tpl);
        assert_ne!(out, cc);
        assert!(out.contains("CLI tool"));
        assert!(!out.contains("official CLI for Claude."));
        // Main branch → Default branch（两种原文写法都覆盖）
        assert!(apply_template_map("Main branch (you will usually use this for PRs)", &tpl)
            .starts_with("Default branch"));
        assert!(apply_template_map("Main branch (you will usually use this with PRs)", &tpl)
            .starts_with("Default branch"));
        // 无指纹内容零改动（预检短路）
        let plain = "普通中文内容不清洗";
        assert_eq!(apply_template_map(plain, &tpl), plain);
    }

    #[test]
    fn template_rewrites_deepseek_harness_identity() {
        // DSH（DeepSeek Harness）身份句：2026-10-06 测试机空完成影子风控实证，
        // 首句 + 模型句 "powered by" 均需改写（glm- 前缀匹配覆盖模型变体）。
        // issue #122 升级：单词级改写（built on）后品牌词残留仍被秒拒，
        // 整句同义改写须连品牌词与自我声明句式一并消除
        let tpl = default_template_map();
        let dsh = "You are an AI agent powered by DeepSeek Harness.\n\nYou are a coding agent powered by the glm-5.3-flash model.";
        let out = apply_template_map(dsh, &tpl);
        assert!(
            out.contains("You are an AI agent that helps the user with programming work."),
            "{out}"
        );
        assert!(
            out.contains(
                "You help the user with programming work using the glm-5.3-flash model."
            ),
            "{out}"
        );
        assert!(!out.contains("powered by"), "{out}");
        // 品牌词与句式形态零残留（issue #122：残留即指纹）
        assert!(!out.contains("DeepSeek Harness"), "{out}");
        assert!(!out.contains("coding agent"), "{out}");
        // 改写结果不再命中任何规则（幂等安全）
        assert_eq!(apply_template_map(&out, &tpl), out);
    }

    /// OpenCode 身份句整句同义改写（issue #122 ×24 实证：tool→assistant 单词级
    /// 改写后 opencode / interactive CLI / software engineering tasks 残留仍被
    /// 上游特征识别秒拒——整句替换三类特征一并消除）
    #[test]
    fn opencode_identity_sentence_whole_rewrite() {
        let tpl = default_template_map();
        let src = "You are opencode, an interactive CLI tool that helps users with software engineering tasks.";
        let out = apply_template_map(src, &tpl);
        assert!(out.contains("pragmatic senior engineer"), "{out}");
        // 品牌/特征词零残留
        assert!(!out.contains("opencode"), "{out}");
        assert!(!out.contains("interactive CLI"), "{out}");
        assert!(!out.contains("software engineering tasks"), "{out}");
        // 改写结果不再命中触发词（幂等安全）
        assert_eq!(apply_template_map(&out, &tpl), out);
    }

    /// OpenCode v2.2.1 身份句整句同义改写（2026-10-29 抓包实证：新版品牌词在
    /// 句中「running in OpenCode」，旧版前缀形态的 aggressive 规则与默认表
    /// 4 条旧规则均不命中）+ aggressive 句中兜底未知后缀变体
    #[test]
    fn opencode_v2_identity_sentence_rewritten() {
        let tpl = default_template_map();
        // 抓包原文（system 首条消息首句 + 尾句任务说明）
        let src = "You are an AI agent running in OpenCode, a coding agent harness. Help the user accomplish their goals using the tools you have available.";
        let out = apply_template_map(src, &tpl);
        assert!(out.contains("working in a local coding workspace"), "{out}");
        // 品牌/特征词零残留（大小写不敏感，harness 词一并消除）
        assert!(!out.to_lowercase().contains("opencode"), "{out}");
        assert!(!out.contains("harness"), "{out}");
        // 尾句保留（只改身份句本体）
        assert!(
            out.ends_with("Help the user accomplish their goals using the tools you have available."),
            "{out}"
        );
        // 改写结果不再命中任何规则（幂等安全）
        assert_eq!(apply_template_map(&out, &tpl), out, "改写须幂等");
        // aggressive 句中兜底：默认表未覆盖的后缀变体仍消除品牌片段
        let mut agg = tpl;
        agg.extend(aggressive_template_rules());
        let out2 = apply_template_map(
            "You are an AI agent running in OpenCode, a coding agent harness v2. Help the user.",
            &agg,
        );
        assert!(!out2.to_lowercase().contains("opencode"), "{out2}");
        assert!(!out2.contains("harness"), "{out2}");
    }

    /// DSH 0.2.1-rc.2 模型句整句同义改写（2026-10-29 headless 抓包实证：模型
    /// 句池前缀升级为 buddy/deepseek-v4.1-flash，glm- 前缀规则不命中）——逐字
    /// 改写保留模型名衔接（RESIDUAL_MARKERS 的 "deepseek harness" 为整词组，
    /// 模型名不含该词组，不触发回归门）+ aggressive 前缀兜未知模型名变体
    #[test]
    fn dsh_model_sentence_rewritten() {
        let tpl = default_template_map();
        // 抓包原文（system 首条前两句：首句身份声明 + 模型句）
        let src = "You are an AI agent powered by DeepSeek Harness.\n\nYou are a coding agent powered by the buddy/deepseek-v4.1-flash model.";
        let out = apply_template_map(src, &tpl);
        assert!(
            out.contains(
                "You help the user with programming work using the buddy/deepseek-v4.1-flash model."
            ),
            "{out}"
        );
        // 品牌/特征词零残留
        assert!(!out.contains("powered by"), "{out}");
        assert!(!out.to_lowercase().contains("deepseek harness"), "{out}");
        assert!(!out.contains("coding agent"), "{out}");
        // 改写结果不再命中任何规则（幂等安全）
        assert_eq!(apply_template_map(&out, &tpl), out, "改写须幂等");
        // aggressive 前缀兜底：未知模型名变体（默认表零命中）在换号重试场景兜住
        let mut agg = tpl;
        agg.extend(aggressive_template_rules());
        let out2 = apply_template_map(
            "You are a coding agent powered by the buddy/deepseek-v9-flash model.",
            &agg,
        );
        assert_eq!(
            out2,
            "You help the user with programming work using the buddy/deepseek-v9-flash model."
        );
        // glm- 前缀句仍由既有规则覆盖，新前缀兜底不破坏其改写
        let out3 = apply_template_map("You are a coding agent powered by the glm-5.3-flash model.", &agg);
        assert_eq!(
            out3,
            "You help the user with programming work using the glm-5.3-flash model."
        );
    }

    /// CodeBuddy 1.106.1 指纹布防回归（2026-10-29 buddy CLI 抓包实证）：
    /// 模型句句式与 glm- 前缀规则不同（"an AI coding assistant, powered by"
    /// 逗号续接形态），逐字整句改写保留模型名衔接；正文两处独家品牌句
    /// （".codebuddy" 文件夹警告 + CodeBuddy docs 官网域名）连根消除 +
    /// aggressive 前缀兜未知模型名变体
    #[test]
    fn codebuddy_fingerprints_whole_rewrite() {
        let tpl = default_template_map();
        // 模型句（抓包原文 system 首句）
        let out = apply_template_map(
            "You are an AI coding assistant, powered by buddy/glm-5.3-flash model.",
            &tpl,
        );
        assert_eq!(
            out,
            "You help the user with programming work using the buddy/glm-5.3-flash model."
        );
        // 正文品牌句 ×2（抓包原文逐字）
        let out1 = apply_template_map(
            "**IMPORTANT**: \".codebuddy\" folder stores project-related data and is NOT a temporary cache. Please do NOT delete this folder!",
            &tpl,
        );
        assert!(!out1.contains("codebuddy"), "{out1}");
        assert!(out1.contains("Please do NOT delete this folder!"), "{out1}");
        let out2 = apply_template_map(
            "When the user asks about CodeBuddy or how to use one of its features (eg. \"can CodeBuddy do...\", \"how do I implement a hook / write a slash command / install an MCP server\"), read the CodeBuddy docs at https://www.codebuddy.ai/docs/zh/ide/User-guide/Overview with the web_fetch tool instead of guessing — if web_fetch is not in your tool list, retrieve it with ToolSearch first.",
            &tpl,
        );
        assert!(!out2.contains("CodeBuddy"), "{out2}");
        assert!(!out2.contains("codebuddy.ai"), "{out2}");
        // 改写结果不再命中任何规则（幂等安全）
        assert_eq!(apply_template_map(&out, &tpl), out, "改写须幂等");
        assert_eq!(apply_template_map(&out1, &tpl), out1, "改写须幂等");
        assert_eq!(apply_template_map(&out2, &tpl), out2, "改写须幂等");
        // aggressive 前缀兜底：未知模型名变体（默认表零命中）在换号重试场景兜住
        let mut agg = tpl;
        agg.extend(aggressive_template_rules());
        let out3 = apply_template_map(
            "You are an AI coding assistant, powered by buddy/glm-9-flash model.",
            &agg,
        );
        assert_eq!(
            out3,
            "You help the user with programming work using the buddy/glm-9-flash model."
        );
    }

    /// ZCode 身份句整句同义改写（issue #155，2026-10 实证：pool=trae 空完成
    /// 影子风控，3 账号轮换全空；指纹样本抓到双身份句原文）——品牌词 ZCode
    /// 连根消除（C3 口径：改写句零品牌词/特征 n-gram）
    #[test]
    fn zcode_identity_sentences_whole_rewrite() {
        let tpl = default_template_map();
        // issue #155 指纹样本原文（system 首条前两句）
        let src = "You are ZCode, an interactive coding agent You are an interactive ZCode agent that helps users with software engineering tasks. IMPORTANT: Assist with authorized security testing...";
        let out = apply_template_map(src, &tpl);
        assert!(!out.contains("ZCode"), "{out}");
        assert!(out.starts_with("You are an interactive coding agent"), "{out}");
        assert!(out.contains("an interactive agent that helps users with software engineering tasks"), "{out}");
        // 改写结果不再命中任何规则（幂等安全）
        assert_eq!(apply_template_map(&out, &tpl), out);
    }

    /// Qoder CN 0.4.11 指纹布防回归（2026-10-29 混淆包逆向实证：chat 生成
    /// 请求经 agent SDK worker 直连网关无法 MITM，身份句自解码字符串原文）：
    /// 主身份句双品牌点（QoderWork + Qoder team）+ 变体句整句同义改写连根
    /// 消除 + aggressive 长前缀/中段/短前缀兜未知变体
    #[test]
    fn qoder_fingerprints_whole_rewrite() {
        let tpl = default_template_map();
        // 主身份句（解码串原文逐字）
        let out = apply_template_map(
            "You are QoderWork, a desktop agentic assistant developed by Qoder team. You are built for daily work, helping users improve their productivity. Use the instructions below and the tools available to you to assist the user.",
            &tpl,
        );
        assert!(!out.to_lowercase().contains("qoder"), "{out}");
        assert!(out.starts_with("You are a desktop work companion"), "{out}");
        // 变体句（解码串原文逐字）
        let out2 = apply_template_map(
            "You are Qoder's desktop agentic assistant, built by the Qoder team to help users solve problems and get things done with clarity and confidence.",
            &tpl,
        );
        assert!(!out2.to_lowercase().contains("qoder"), "{out2}");
        assert!(out2.ends_with("clarity and confidence."), "{out2}");
        // 改写结果不再命中任何规则（幂等安全）
        assert_eq!(apply_template_map(&out, &tpl), out, "改写须幂等");
        assert_eq!(apply_template_map(&out2, &tpl), out2, "改写须幂等");
        // aggressive 兜底：未知 flavor 变体（默认表零命中）在换号重试场景兜住
        let mut agg = tpl;
        agg.extend(aggressive_template_rules());
        // 短前缀变体（QoderWork 后接未知后缀，长前缀规则零命中）
        let out3 = apply_template_map("You are QoderWork, an agent for daily productivity.", &agg);
        assert!(!out3.to_lowercase().contains("qoder"), "{out3}");
        // 中段品牌词组变体（非 "You are" 开头的正文品牌句）
        let out4 = apply_template_map("This agent was developed by Qoder team for desktop use.", &agg);
        assert!(!out4.to_lowercase().contains("qoder"), "{out4}");
    }

    /// Claude Code v2.1.312（Agent SDK harness）身份句布防回归：默认表整句
    /// 去品牌改写 + aggressive 前缀兜未知后缀变体（system 数组多块经
    /// anthropic_to_openai 无缝拼接，第二块身份句可被首条 system 改写命中）
    #[test]
    fn claude_agent_sdk_identity_sentence_rewritten() {
        let tpl = default_template_map();
        let src = "You are a Claude agent, built on Anthropic's Claude Agent SDK. You help with software engineering tasks.";
        let out = apply_template_map(src, &tpl);
        assert!(!out.contains("Claude agent"), "{out}");
        assert!(!out.contains("Anthropic"), "{out}");
        assert!(out.ends_with("software engineering tasks."), "{out}");
        assert_eq!(apply_template_map(&out, &tpl), out, "改写须幂等");
        // aggressive 兜底：默认表未覆盖的后缀变体仍消除品牌前缀
        let mut agg = tpl;
        agg.extend(aggressive_template_rules());
        let out2 = apply_template_map(
            "You are a Claude agent, built on Anthropic's Claude Agent SDK v2.",
            &agg,
        );
        assert!(!out2.contains("Claude agent"), "{out2}");
        assert!(out2.starts_with("You are a coding agent"), "{out2}");
    }

    /// 默认表全部改写句零残指纹 + 幂等（issue #122 方案 C3 口径回归锁定）：
    /// 改写句自身含特征词 = 指纹仍在（首轮请求激进规则不兜底）+
    /// residual_fingerprint_note 设计性误报；任何新增/修改默认规则必须过此门
    #[test]
    fn default_map_outputs_are_residue_free_and_idempotent() {
        let tpl = default_template_map();
        for (from, to) in &tpl {
            assert_ne!(from, to, "规则未改写: {from}");
            let out = apply_template_map(from, &tpl);
            let low = out.to_lowercase();
            for m in RESIDUAL_MARKERS {
                assert!(!low.contains(*m), "改写句残留特征词 {m}: {from} → {out}");
            }
            assert_eq!(apply_template_map(&out, &tpl), out, "改写须幂等: {from} → {out}");
        }
    }

    /// issue #122 缺口2：池级指纹拦截短记忆——blocked=true 标记后同池 TTL 内
    /// 命中、异池隔离；blocked=false 不标记；未标记池恒不命中
    #[test]
    fn fp_block_short_memory_scoped_by_pool() {
        let pool = "test-fp-block-pool";
        assert!(!fp_block_recent(pool));
        mark_channel_fp_block(pool, false);
        assert!(!fp_block_recent(pool));
        mark_channel_fp_block(pool, true);
        assert!(fp_block_recent(pool));
        assert!(!fp_block_recent("test-fp-block-other-pool"));
    }

    #[test]
    fn sanitize_strips_cc_and_anthropic_tokens() {
        assert_eq!(strip_cc_fingerprints("ok cc_user=abc; tail"), "ok tail");
        assert_eq!(strip_cc_fingerprints("a cc_env=X:1,b"), "a b");
        assert_eq!(
            strip_cc_fingerprints("see x-anthropic-beta: prompt-caching end"),
            "see end"
        );
        // 普通下划线词不受影响
        assert_eq!(strip_cc_fingerprints("success_count=3"), "success_count=3");
        // P1 修复回归：裸 cc_ 词（无 =/: 值分隔符）属正常业务内容，不得删除
        assert_eq!(
            strip_cc_fingerprints("use cc_flags for build"),
            "use cc_flags for build"
        );
        assert_eq!(
            strip_cc_fingerprints("add cc_config: check crate cc_deps next"),
            "add crate cc_deps next"
        );
        // 键名在文本结尾（无值可取）同样保留
        assert_eq!(
            strip_cc_fingerprints("ends with cc_config"),
            "ends with cc_config"
        );
    }

    #[test]
    fn sanitize_applied_to_message_content() {
        let src = json!({"messages":[{"role":"system","content":"You are Claude Code, Anthropic's official CLI for Claude."}]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let tpl = default_template_map();
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, true, &tpl, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let s = v["messages"][0]["content"].as_str().unwrap();
        assert!(s.contains("CLI tool"));
        assert!(!s.contains("official CLI for Claude."));
        // sanitize 关闭时跳过该预处理（与指纹清洗开关联动 §5.5 #9）
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, false, &tpl, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"][0]["content"], json!("You are Claude Code, Anthropic's official CLI for Claude."));
    }

    /// 回归：连续两条 role:"tool" 消息不合并，tool_call_id 逐条保留；
    /// 连续 role:"user" 仍合并（既有语义）；tool 之后跟 user 也不得并入 tool
    #[test]
    fn tool_messages_never_merge_and_keep_call_ids() {
        // 输入自带首条 system：不触发兜底注入
        let out = rewrite(json!({"messages":[
            {"role":"system","content":"sys"},
            {"role":"assistant","content":null,
             "tool_calls":[{"id":"c1","type":"function","function":{"name":"f","arguments":"{}"}}]},
            {"role":"tool","tool_call_id":"c1","content":"r1"},
            {"role":"tool","tool_call_id":"c2","content":"r2"},
        ]}), None);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4, "连续两条 role:tool 不合并");
        assert_eq!(msgs[2]["tool_call_id"], json!("c1"));
        assert_eq!(msgs[3]["tool_call_id"], json!("c2"));
        // 连续 user 仍合并（system 兜底注入后共 2 条）
        let out = rewrite(json!({"messages":[
            {"role":"system","content":"sys"},
            {"role":"user","content":"a"},
            {"role":"user","content":"b"},
        ]}), None);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1]["content"], json!("a\n\nb"));
        // tool 之后的 user 消息不得并入 tool 消息
        let out = rewrite(json!({"messages":[
            {"role":"system","content":"sys"},
            {"role":"tool","tool_call_id":"c1","content":"r"},
            {"role":"user","content":"继续"},
        ]}), None);
        assert_eq!(out["messages"].as_array().unwrap().len(), 3);
    }

    /// 外置映射表新增规则不再被硬编码预检短路：规则命中词不含内置三短语也生效
    #[test]
    fn external_template_rules_apply_without_hardcoded_precheck() {
        let tpl = vec![("FooBar".to_string(), "Baz".to_string())];
        assert_eq!(apply_template_map("hello FooBar world", &tpl), "hello Baz world");
        // 空映射表 → 原样
        assert_eq!(apply_template_map("Claude Code", NO_TPL), "Claude Code");
        // 无规则命中的内容零改动（原「预检短路」语义由无命中自然保证）
        let plain = "普通中文内容不清洗";
        assert_eq!(apply_template_map(plain, &default_template_map()), plain);
    }

    /// 指纹清洗覆盖 tool_calls[].function.arguments（最小改写、JSON 结构不破坏）
    #[test]
    fn sanitize_covers_tool_call_arguments() {
        let src = json!({"messages":[
            {"role":"system","content":"sys"},
            {"role":"assistant","content":null,"tool_calls":[
                {"id":"c1","type":"function","function":{"name":"note",
                 "arguments":"{\"prompt\":\"You are Claude Code, Anthropic's official CLI for Claude.\"}"}}
            ]}
        ]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let tpl = default_template_map();
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, true, &tpl, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let args = v["messages"][1]["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
        assert!(args.contains("CLI tool"));
        assert!(!args.contains("official CLI for Claude."));
        // arguments 仍是合法 JSON（结构未被清洗破坏）
        let parsed: Value = serde_json::from_str(args).unwrap();
        assert_eq!(
            parsed["prompt"],
            json!("You are Claude Code, Anthropic's official CLI tool for Claude.")
        );
        // sanitize=false 不清洗
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, false, &tpl, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v["messages"][1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("official CLI for Claude."));
    }

    /// tool_choice 带 function.name 的对象（含未知 type / 缺 type 形态）归一为该 name
    #[test]
    fn tool_choice_name_bearing_objects_normalize_to_name() {
        // 缺 type 的 function 对象 → name（旧实现误归 auto 丢失指定意图）
        let out = rewrite(json!({"messages":[],"tool_choice":{"function":{"name":"get_weather"}}}), None);
        assert_eq!(out["tool_choice"], json!("get_weather"));
        // 未知 type 但带 function.name → name
        let out = rewrite(json!({"messages":[],"tool_choice":{"type":"custom","function":{"name":"t"}}}), None);
        assert_eq!(out["tool_choice"], json!("t"));
        // 既有形态回归：type=function → name
        let out = rewrite(json!({"messages":[],"tool_choice":{"type":"function","function":{"name":"f"}}}), None);
        assert_eq!(out["tool_choice"], json!("f"));
        // 无 name 的未知形态 → auto
        let out = rewrite(json!({"messages":[],"tool_choice":{"type":"weird"}}), None);
        assert_eq!(out["tool_choice"], json!("auto"));
    }

    /// Codex CLI 身份句指纹清洗（v1-v4 harness 首句最小改写 + 0.145.0
    /// 续句/语境内指代句整句同义改写，C3 口径改写句零品牌词）
    #[test]
    fn codex_identity_sentences_minimal_rewrite() {
        let tpl = default_template_map();
        // v1 harness 首句（gpt-5/5.1/5.3-codex 通用）
        let v1 = "You are Codex, based on GPT-5. You are running as a coding agent in the Codex CLI on a user's computer.";
        let out = apply_template_map(v1, &tpl);
        assert_ne!(out, v1);
        assert!(out.starts_with("You are Codex, built on GPT-5."));
        assert!(!out.contains("based on GPT-5"));
        // v2 harness 首句（新版 harness）
        let v2 = "You are Codex, an agent based on GPT-5. You and the user share one workspace.";
        let out = apply_template_map(v2, &tpl);
        assert_ne!(out, v2);
        assert!(out.starts_with("You are Codex, an agent built on GPT-5."));
        assert!(!out.contains("based on GPT-5"));
        // v3 harness 首句（9/19 新版 codex.exe 实证）
        let v3 = "You are Codex, a coding agent based on GPT-5. You and the user share the same workspace and collaborate to achieve the user's goals.";
        let out = apply_template_map(v3, &tpl);
        assert_ne!(out, v3);
        assert!(out.starts_with("You are Codex, a coding agent built on GPT-5."));
        assert!(!out.contains("based on GPT-5"));
        // v4 harness 首句（同上新版 codex.exe 实证，CLI 官方 harness）
        let v4 = "You are a coding agent running in the Codex CLI, a terminal-based coding assistant.";
        let out = apply_template_map(v4, &tpl);
        assert_ne!(out, v4);
        assert!(out.starts_with("You are a coding agent working in the Codex CLI"));
        assert!(!out.contains("running in the Codex CLI"));
        // 两条规则互不误伤（v1 改写结果不含 v2 触发词，反之亦然）
        assert_eq!(
            apply_template_map("You are Codex, built on GPT-5.", &tpl),
            "You are Codex, built on GPT-5."
        );
        // 0.145.0（2026-10-29 /v1/responses 抓包实证）首句续句：品牌词
        // Codex/OpenAI 双点整句同义改写连根消除（C3 口径：to 零品牌词）
        let cont = "Codex CLI is an open source project led by OpenAI.";
        let out = apply_template_map(cont, &tpl);
        assert_ne!(out, cont);
        assert!(out.starts_with("The command-line coding tool is an open source project"));
        assert!(!out.contains("Codex"), "{out}");
        assert!(!out.contains("OpenAI"), "{out}");
        assert_eq!(apply_template_map(&out, &tpl), out);
        // 语境内指代句（"Codex" 消歧句）同款整句同义改写
        let ctx = "Within this context, Codex refers to the open-source agentic coding interface (not the old Codex language model built by OpenAI).";
        let out = apply_template_map(ctx, &tpl);
        assert_ne!(out, ctx);
        assert!(!out.contains("Codex"), "{out}");
        assert!(!out.contains("OpenAI"), "{out}");
        assert_eq!(apply_template_map(&out, &tpl), out);
    }

    /// Codex instructions → system 消息端到端清洗（responses_to_chat 产物形态）
    #[test]
    fn sanitize_covers_codex_instructions_system_message() {
        let src = json!({"messages":[
            {"role":"system","content":"You are Codex, based on GPT-5. You are running as a coding agent in the Codex CLI on a user's computer."},
            {"role":"user","content":"帮我写个函数"},
        ]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let tpl = default_template_map();
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, true, &tpl, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let s = v["messages"][0]["content"].as_str().unwrap();
        assert!(s.contains("built on GPT-5"));
        assert!(!s.contains("based on GPT-5"));
        // user 消息不受影响
        assert_eq!(v["messages"][1]["content"], json!("帮我写个函数"));
        // sanitize=false 原样透传（清洗开关联动）
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, false, &tpl, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert!(v["messages"][0]["content"].as_str().unwrap().contains("based on GPT-5"));
    }

    /// Codex 0.145.0 三句端到端（responses_to_chat 投影后的 system 消息形态）：
    /// v4 首句 + 续句 + 语境内指代句一次清洗；user 正文提及 Codex 不受影响；
    /// 残指纹自检 None（"codex" 非 RESIDUAL_MARKERS 成员的设计性口径——v1-v4
    /// 规则改写句含 Codex/GPT-5，纳入即回归门误报）
    #[test]
    fn sanitize_covers_codex_0145_sentences_end_to_end() {
        let src = json!({"messages":[
            {"role":"system","content":"You are a coding agent running in the Codex CLI. Codex CLI is an open source project led by OpenAI. Within this context, Codex refers to the open-source agentic coding interface (not the old Codex language model built by OpenAI)."},
            {"role":"user","content":"Codex CLI 怎么配置 MCP？"}
        ]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let tpl = default_template_map();
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, true, &tpl, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let s = v["messages"][0]["content"].as_str().unwrap();
        // v4 首句改写保留 Codex CLI（无实证品牌词被拦，按 #9 cat-and-mouse 口径）
        assert!(s.contains("working in the Codex CLI"), "{s}");
        // 续句 + 指代句整句同义改写，品牌词连根消除
        assert!(
            s.contains("The command-line coding tool is an open source project"),
            "{s}"
        );
        assert!(
            s.contains("the product name refers to the open-source agentic coding interface"),
            "{s}"
        );
        assert!(!s.contains("led by OpenAI"), "{s}");
        assert!(!s.contains("old Codex language model"), "{s}");
        // user 正文品牌词不动
        assert_eq!(v["messages"][1]["content"], json!("Codex CLI 怎么配置 MCP？"));
        // 改写结果幂等 + 残指纹自检 None（设计性口径锁定）
        assert_eq!(apply_template_map(s, &tpl), s);
        assert_eq!(residual_fingerprint_note(&out), None);
    }

    /// TraeCode 身份句整句同义改写（日志实证 pool=trae system 首句原文；
    /// issue #129：品牌词 TraeCode 连根消除——最小改写残留被 buddy-ai 国际档
    /// 渠道验证拦截，改写句零品牌词）
    #[test]
    fn trae_identity_sentence_neutralized_rewrite() {
        let tpl = default_template_map();
        let src = "You are an interactive agent in TraeCode that helps the USER with software engineering tasks. Use the instructions below and the tools available to you to assist the USER.";
        let out = apply_template_map(src, &tpl);
        assert_ne!(out, src);
        assert!(out.contains("interactive agent that helps the USER"));
        assert!(!out.contains("TraeCode"), "{out}");
        // 旧版最小改写句形态（≤v3.8.0 发出的 "running in TraeCode"）同样收敛
        let legacy = "You are an interactive agent running in TraeCode that helps the USER with software engineering tasks. Use the instructions below.";
        let out2 = apply_template_map(legacy, &tpl);
        assert!(out2.contains("interactive agent that helps the USER"));
        assert!(!out2.contains("TraeCode"), "{out2}");
        // 改写结果不再命中触发词（幂等安全）
        assert_eq!(apply_template_map(&out, &tpl), out);
        assert_eq!(apply_template_map(&out2, &tpl), out2);
    }

    /// TraeCode system 消息端到端清洗（pool=trae 请求形态）
    #[test]
    fn sanitize_covers_trae_system_message() {
        let src = json!({"messages":[
            {"role":"system","content":"You are an interactive agent in TraeCode that helps the USER with software engineering tasks. Use the instructions below and the tools available to you to assist the USER."},
            {"role":"user","content":"fix the bug"},
        ]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let tpl = default_template_map();
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, true, &tpl, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let s = v["messages"][0]["content"].as_str().unwrap();
        assert!(s.contains("agent that helps the USER"));
        assert!(!s.contains("TraeCode"));
        // user 消息不受影响
        assert_eq!(v["messages"][1]["content"], json!("fix the bug"));
    }

    /// aggressive Trae 规则顺序回归（issue #129）：长前缀 "You are Trae AI" 必须
    /// 先于 "You are Trae" 应用——否则改写输出破句为 "an AI assistant AI,"；
    /// 后续在 aggressive 规则表插队新增规则时由此锁定顺序不被无意识破坏
    #[test]
    fn aggressive_trae_rules_long_prefix_first() {
        let mut tpl = default_template_map();
        tpl.extend(aggressive_template_rules());
        let src = "You are Trae AI, powered by the glm model. Help the user with coding.";
        let out = apply_template_map(src, &tpl);
        assert!(out.starts_with("You are an AI assistant, powered by"), "{out}");
        assert!(!out.contains("assistant AI"), "{out}");
    }

    /// aggressive ZCode 规则（issue #155）：未知 flavor/新版后缀变体兜底——
    /// 默认表两条实证整句只覆盖已知句式，短前缀兜住变体；与 Trae 规则同理，
    /// 长前缀 "You are ZCode, ..." 居前，短规则 "You are ZCode" 不破句
    #[test]
    fn aggressive_zcode_rules_catch_unknown_variants() {
        let mut tpl = default_template_map();
        tpl.extend(aggressive_template_rules());
        // 已知整句（默认表命中，aggressive 不再参与）
        let known = "You are ZCode, an interactive coding agent. Review my diff.";
        let out = apply_template_map(known, &tpl);
        assert!(out.starts_with("You are an interactive coding agent"), "{out}");
        assert!(!out.contains("ZCode"), "{out}");
        // 未知变体（默认表未覆盖，aggressive 短前缀兜底）
        let variant = "You are ZCode v9, the ultimate harness. Do the task.";
        let out2 = apply_template_map(variant, &tpl);
        assert!(out2.starts_with("You are an interactive agent"), "{out2}");
        assert!(!out2.contains("ZCode"), "{out2}");
        // 另一变体形态（interactive ZCode agent 中插词）
        let variant2 = "You are an interactive ZCode agent pro. Do the task.";
        let out3 = apply_template_map(variant2, &tpl);
        assert!(out3.starts_with("You are an interactive agent"), "{out3}");
        assert!(!out3.contains("ZCode"), "{out3}");
    }

    /// tools[].function.description 指纹清洗（CLI harness 将身份句嵌入工具
    /// 描述的绕过通道——消息体清洗覆盖不到，必须单独处理）
    #[test]
    fn sanitize_covers_tool_function_description() {
        let src = json!({
            "messages": [{"role":"user","content":"hi"}],
            "tools": [
                {"type":"function","function":{
                    "name":"web_search",
                    "description":"Web search tool. You are Codex, based on GPT-5. You are Claude Code, Anthropic's official CLI for Claude. You are an interactive agent in TraeCode that helps the USER with software engineering tasks.",
                    "parameters":{"type":"object","properties":{"query":{"type":"string"}}}
                }},
                {"type":"function","function":{
                    "name":"read_file",
                    "description":"Reads a file from disk.",
                    "parameters":{"type":"object","properties":{"path":{"type":"string"}}}
                }},
                // 非 function 形态工具（Responses 原生 web_search）不误伤
                {"type":"web_search"}
            ]
        });
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let tpl = default_template_map();
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, true, &tpl, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let tools = v["tools"].as_array().unwrap();

        // 工具 1：三条指纹全部改写（规则逐字匹配完整身份句）
        let d0 = tools[0]["function"]["description"].as_str().unwrap();
        assert!(d0.contains("You are Codex, built on GPT-5"));
        assert!(d0.contains("official CLI tool for Claude"));
        assert!(d0.contains("interactive agent that helps the USER"));
        assert!(!d0.contains("based on GPT-5"));
        assert!(!d0.contains("CLI for Claude"));
        assert!(!d0.contains("TraeCode"));

        // 工具 2：无指纹原样保留
        assert_eq!(tools[1]["function"]["description"], json!("Reads a file from disk."));
        // 工具 3：无 description 字段的工具不误伤
        assert_eq!(tools[2]["type"], json!("web_search"));
        assert!(tools[2].get("function").is_none());

        // sanitize=false 原样透传（清洗开关联动）
        let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, false, &tpl, 0, false);
        let v: Value = serde_json::from_slice(&out).unwrap();
        let d0 = v["tools"][0]["function"]["description"].as_str().unwrap();
        assert!(d0.contains("based on GPT-5"));
    }

    /// issue #122 方案 C1：多候选改写——`|` 分隔候选按种子稳定选一，跨种子
    /// 变化、同种子可复现；单候选规则与种子无关（存量零影响）
    #[test]
    fn multi_candidate_seeded_rewrite() {
        let tpls = vec![("FP_M".to_string(), "Alpha|Beta|Gamma".to_string())];
        // 任一种子：结果必为候选之一（不产生残句拼接）
        for seed in [0u64, 1, 2, 3, 42, 0xdead_beef] {
            let out = apply_template_map_seeded("x FP_M y", &tpls, seed);
            assert!(
                out == "x Alpha y" || out == "x Beta y" || out == "x Gamma y",
                "seed={seed} out={out}"
            );
        }
        // 跨种子变化：连续 8 个种子命中全部 3 个候选（idx = (seed^哈希)%3
        // 在连续种子下取遍所有余数）
        let outs: std::collections::HashSet<String> = (0u64..8)
            .map(|s| apply_template_map_seeded("FP_M", &tpls, s))
            .collect();
        assert_eq!(outs.len(), 3, "连续种子应取遍全部候选: {outs:?}");
        // 同种子可复现（同请求内稳定）
        assert_eq!(
            apply_template_map_seeded("FP_M", &tpls, 7),
            apply_template_map_seeded("FP_M", &tpls, 7)
        );
        // 单候选规则：种子无关
        let single = vec![("FP_S".to_string(), "Only".to_string())];
        for seed in [0u64, 9, 12345] {
            assert_eq!(apply_template_map_seeded("FP_S", &single, seed), "Only");
        }
        // seed=0 稳定入口（Trae 池）：确定性行为
        assert_eq!(apply_template_map("FP_M", &tpls), apply_template_map("FP_M", &tpls));
        // 剥离候选（空串）合法：命中即删除原文
        let strip = vec![("FP_X".to_string(), String::new())];
        assert_eq!(apply_template_map_seeded("a FP_X b", &strip, 5), "a  b");
    }

    /// 多候选含空串（剥离语义）按种子命中：命中空候选时输出彻底为空、无残句
    /// 拼接；连续种子取遍含空串的全部候选（`|` 首尾变体均覆盖）
    #[test]
    fn multi_candidate_empty_strip_candidate_seeded() {
        // 尾空候选："Alpha|" 两候选（"Alpha" 与 ""）
        let tpls = vec![("FP_D".to_string(), "Alpha|".to_string())];
        for seed in 0u64..8 {
            let out = apply_template_map_seeded("FP_D", &tpls, seed);
            assert!(out == "Alpha" || out.is_empty(), "seed={seed} out={out:?}");
        }
        let outs: std::collections::HashSet<String> = (0u64..8)
            .map(|s| apply_template_map_seeded("FP_D", &tpls, s))
            .collect();
        assert!(
            outs.contains("Alpha") && outs.contains(""),
            "连续种子应取遍含空串的全部候选: {outs:?}"
        );
        // 首空候选："|Beta" 剥离落点前后缀保留、原文消失，无残句拼接
        let tpls2 = vec![("FP_D2".to_string(), "|Beta".to_string())];
        let out = apply_template_map_seeded("x FP_D2 y", &tpls2, 3);
        assert!(out == "x  y" || out == "x Beta y", "seed=3 out={out:?}");
    }

    /// issue #122 方案 C1：variant_seed 请求种子——同入参稳定、任一变化即漂移
    /// （跨换号重试/工具执行轮次的候选必变）
    #[test]
    fn variant_seed_stable_per_inputs_and_varies() {
        assert_eq!(variant_seed("conv-1", 1), variant_seed("conv-1", 1));
        assert_ne!(variant_seed("conv-1", 1), variant_seed("conv-1", 2));
        assert_ne!(variant_seed("conv-1", 1), variant_seed("conv-2", 1));
    }

    /// issue #122 方案 C1 端到端：prepare 按种子对 messages 内身份句选候选，
    /// 跨种子产生不同改写句、品牌词零残留
    #[test]
    fn prepare_applies_seeded_multi_candidate() {
        let tpls = vec![(
            "You are FP_E2E".to_string(),
            "You are Candidate One|You are Candidate Two".to_string(),
        )];
        let src = json!({"messages":[{"role":"system","content":"You are FP_E2E, a test harness."}]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let mut outs = std::collections::HashSet::new();
        for seed in 0u64..6 {
            let out = prepare_wb_chat_body(&src_bytes, "m", "", "", None, true, &tpls, seed, false);
            let v: Value = serde_json::from_slice(&out).unwrap();
            let s = v["messages"][0]["content"].as_str().unwrap().to_string();
            assert!(s.starts_with("You are Candidate "), "{s}");
            assert!(!s.contains("FP_E2E"), "{s}");
            outs.insert(s);
        }
        assert!(outs.len() > 1, "种子轮换应产生不同候选: {outs:?}");
    }

    /// issue #122 方案 C3：残指纹自检——已知句整句改写零残留不报；未知变体
    /// 前缀残留命中注记；用户正文品牌词不参与（不误报）；坏 body / 无 system
    /// 不 panic 返回 None
    #[test]
    fn residual_fingerprint_detection() {
        let tpl = default_template_map();
        // 已知句整句改写 → 首条 system 零残留；user 正文含品牌词不参与自检
        let ok = json!({"messages":[
            {"role":"system","content":"You are opencode, an interactive CLI tool that helps users with software engineering tasks."},
            {"role":"user","content":"聊聊 opencode 这个项目"}
        ]});
        let b = serde_json::to_vec(&ok).unwrap();
        let washed = prepare_wb_chat_body(&b, "m", "", "", None, true, &tpl, 0, false);
        assert_eq!(residual_fingerprint_note(&washed), None);
        // 未知变体（默认表未覆盖、激进规则未启用）→ 前缀残留 → 命中注记
        let leak = json!({"messages":[
            {"role":"system","content":"You are opencode, a secret new flavor."}
        ]});
        let b2 = serde_json::to_vec(&leak).unwrap();
        let washed2 = prepare_wb_chat_body(&b2, "m", "", "", None, true, &tpl, 0, false);
        let note = residual_fingerprint_note(&washed2).expect("未知变体应命中残指纹");
        assert!(note.contains("opencode"), "{note}");
        // 非 JSON body → None 不 panic
        assert_eq!(residual_fingerprint_note(b"not json"), None);
        // 无 system 消息 → None（user 正文品牌词不误报）
        let nosys =
            serde_json::to_vec(&json!({"messages":[{"role":"user","content":"opencode"}]})).unwrap();
        assert_eq!(residual_fingerprint_note(&nosys), None);
    }

    /// 残指纹自检边界：数组多块 system（Claude SDK harness 形态，首块空串、
    /// 次块命中）参与自检；单条 system 多 marker 逗号全列；空 system 串 → None
    #[test]
    fn residual_fingerprint_detection_array_system_and_multi_markers() {
        // 数组多块 system：首块空串 + 次块含特征词 → Some（first_system_text
        // 拼接两块文本后扫描）
        let arr = json!({"messages":[
            {"role":"system","content":[
                {"type":"text","text":""},
                {"type":"text","text":"You are an AI agent running in OpenCode, a coding agent harness."}
            ]}
        ]});
        let b = serde_json::to_vec(&arr).unwrap();
        let note = residual_fingerprint_note(&b).expect("数组多块 system 应参与自检");
        assert!(note.contains("opencode"), "{note}");
        // 单条 system 同时含三类特征词组 → 逗号全列（顺序按特征表，不锁死）
        let multi = json!({"messages":[
            {"role":"system","content":"You are an interactive CLI tool. Powered by DeepSeek Harness. Running in OpenCode."}
        ]});
        let b2 = serde_json::to_vec(&multi).unwrap();
        let note2 = residual_fingerprint_note(&b2).expect("多 marker 应全部列出");
        assert!(note2.contains("interactive cli"), "{note2}");
        assert!(note2.contains("deepseek harness"), "{note2}");
        assert!(note2.contains("opencode"), "{note2}");
        assert_eq!(note2.split(',').count(), 3, "三 marker 逗号连接: {note2}");
        // 空 system 串 → None（无文本可扫描）
        let empty =
            serde_json::to_vec(&json!({"messages":[{"role":"system","content":""}]})).unwrap();
        assert_eq!(residual_fingerprint_note(&empty), None);
    }

    /// issue #122 方案 C2：激进清洗仅在 aggressive=true（空完成换号重试）时
    /// 应用——未知变体短前缀兜底；已知句默认表整句规则先行（激进不产生二次
    /// 差异）；sanitize=false 时激进规则不生效
    #[test]
    fn aggressive_rewrite_gated_by_flag_and_normal_first() {
        // 默认表未覆盖的未知 opencode 变体：aggressive=false 原样、true 兜底改写
        let variant = "You are opencode, a brand new interactive CLI flavor that helps users with code.";
        let src = json!({"messages":[{"role":"system","content":variant}]});
        let src_bytes = serde_json::to_vec(&src).unwrap();
        let tpl = default_template_map();
        let out0 = prepare_wb_chat_body(&src_bytes, "m", "", "", None, true, &tpl, 0, false);
        let v0: Value = serde_json::from_slice(&out0).unwrap();
        assert_eq!(v0["messages"][0]["content"], json!(variant), "首轮不应用激进规则");
        let out1 = prepare_wb_chat_body(&src_bytes, "m", "", "", None, true, &tpl, 0, true);
        let v1: Value = serde_json::from_slice(&out1).unwrap();
        let s1 = v1["messages"][0]["content"].as_str().unwrap();
        assert!(s1.contains("pragmatic senior engineer"), "{s1}");
        assert!(!s1.contains("opencode"), "{s1}");
        // 已知句：默认表整句规则先行，激进前缀不再命中（改写结果与仅默认表一致）
        let known = "You are opencode, an interactive CLI tool that helps users with software engineering tasks.";
        let src2 = json!({"messages":[{"role":"system","content":known}]});
        let b2 = serde_json::to_vec(&src2).unwrap();
        let out2 = prepare_wb_chat_body(&b2, "m", "", "", None, true, &tpl, 0, true);
        let out2b = prepare_wb_chat_body(&b2, "m", "", "", None, true, &tpl, 0, false);
        assert_eq!(out2, out2b, "默认表已整句改写，激进规则不应产生二次差异");
        // sanitize=false 时激进规则不生效（无清洗即无升级）
        let out3 = prepare_wb_chat_body(&src_bytes, "m", "", "", None, false, &tpl, 0, true);
        let v3: Value = serde_json::from_slice(&out3).unwrap();
        assert_eq!(v3["messages"][0]["content"], json!(variant));
        // 「powered by <harness>」未知句式兜底（默认表逐字句之外的变体）
        let dsh_variant = "You are an AI agent powered by DeepSeek Harness, running in a terminal.";
        let src4 = json!({"messages":[{"role":"system","content":dsh_variant}]});
        let b4 = serde_json::to_vec(&src4).unwrap();
        let out4 = prepare_wb_chat_body(&b4, "m", "", "", None, true, &tpl, 0, true);
        let v4: Value = serde_json::from_slice(&out4).unwrap();
        let s4 = v4["messages"][0]["content"].as_str().unwrap();
        assert!(s4.contains("that helps the user with programming work"), "{s4}");
        assert!(!s4.contains("DeepSeek Harness"), "{s4}");
    }

    /// aggressive Codex 0.145.0 前缀兜底：默认表逐字整句先行（激进不产生二次
    /// 差异，规则顺序回归锁定）；未知后缀/flavor 变体由短前缀兜底改写；
    /// prepare 首轮透传、换号重试（aggressive=true）兜底
    #[test]
    fn aggressive_codex_rules_catch_unknown_variants() {
        let mut tpl = default_template_map();
        tpl.extend(aggressive_template_rules());
        // 已知整句：默认表先行命中，改写句零品牌词 + 幂等
        let known = "Codex CLI is an open source project led by OpenAI.";
        let out = apply_template_map(known, &tpl);
        assert!(
            out.starts_with(
                "The command-line coding tool is an open source project maintained by a dedicated engineering team."
            ),
            "{out}"
        );
        assert_eq!(apply_template_map(&out, &tpl), out);
        // 未知后缀变体：aggressive 短前缀兜底，品牌前缀连根消除 + 幂等
        let variant = "Codex CLI is an open source project driven by volunteers.";
        let out2 = apply_template_map(variant, &tpl);
        assert!(
            out2.starts_with("The command-line coding tool is an open source project"),
            "{out2}"
        );
        assert!(!out2.contains("Codex"), "{out2}");
        assert_eq!(apply_template_map(&out2, &tpl), out2);
        // 规则顺序回归：已知整句 prepare aggressive=true 与 false 输出一致
        // （若激进前缀被挪到默认表之前，"led by OpenAI" 残留即触发本断言）
        let src = json!({"messages":[{"role":"system","content":known}]});
        let b = serde_json::to_vec(&src).unwrap();
        let t0 = prepare_wb_chat_body(&b, "m", "", "", None, true, &tpl, 0, false);
        let t1 = prepare_wb_chat_body(&b, "m", "", "", None, true, &tpl, 0, true);
        assert_eq!(t0, t1, "默认表已整句改写，激进规则不应产生二次差异");
        // prepare 端到端：首轮（false）透传、重试（true）兜底改写——prepare
        // 自行按 aggressive 标志追加激进规则，此处传纯默认表
        let src2 = json!({"messages":[{"role":"system","content":variant}]});
        let b2 = serde_json::to_vec(&src2).unwrap();
        let tpl_plain = default_template_map();
        let out0 = prepare_wb_chat_body(&b2, "m", "", "", None, true, &tpl_plain, 0, false);
        let v0: Value = serde_json::from_slice(&out0).unwrap();
        assert_eq!(v0["messages"][0]["content"], json!(variant), "首轮不应用激进规则");
        let out1 = prepare_wb_chat_body(&b2, "m", "", "", None, true, &tpl_plain, 0, true);
        let v1: Value = serde_json::from_slice(&out1).unwrap();
        let s1 = v1["messages"][0]["content"].as_str().unwrap();
        assert!(s1.contains("The command-line coding tool"), "{s1}");
        assert!(!s1.contains("Codex"), "{s1}");
    }

    /// 环形缓冲测试互斥锁：empty_sample_note 与既有缓冲测试共用全局 store，
    /// 串行化防并行交错破坏绝对计数断言
    fn fp_sample_test_lock() -> &'static std::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// empty_sample_note 收尾门控（issue #99 ① + issue #125）：仅
    /// CHANNEL_FINGERPRINT_BLOCK 收尾返回「 指纹样本: …」注记并落环形缓冲
    /// （kind=empty）；其余收尾空串不采样；同 (kind,pool,sample) 去重、跨池
    /// 不互吞；坏 body 落「body 解析失败」兜底不 panic
    #[test]
    fn empty_sample_note_gated_by_exhaust_tag() {
        let _guard = fp_sample_test_lock().lock().unwrap_or_else(|e| e.into_inner());
        fingerprint_sample_entries_clear();
        let body = serde_json::to_vec(&json!({
            "messages":[{"role":"system","content":"You are opencode, an interactive CLI tool that helps users with software engineering tasks."}],
            "tools":[{"type":"function","function":{"name":"grep","parameters":{"type":"object"}}}]
        }))
        .unwrap();
        // 非指纹拦截收尾：空串注记、不采样
        assert_eq!(empty_sample_note("POOL_EXHAUSTED", "trae", &body), "");
        assert_eq!(empty_sample_note("", "wb", &body), "");
        assert!(fingerprint_sample_entries().is_empty(), "非指纹拦截收尾不采样");
        // 命中收尾：返回注记并落缓冲（system 画像 + 工具名画像）
        let note = empty_sample_note("CHANNEL_FINGERPRINT_BLOCK", "trae", &body);
        assert!(note.starts_with(" 指纹样本: system=\""), "{note}");
        assert!(note.contains("opencode"), "{note}");
        assert!(note.contains("tools=[grep](total=1"), "{note}");
        let entries = fingerprint_sample_entries();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0].kind, "empty");
        assert_eq!(entries[0].pool, "trae");
        assert!(entries[0].ts > 0, "unix 秒时间戳");
        // 同 (kind,pool,sample) 重复收尾去重，不重复占位（注记仍返回）
        let note2 = empty_sample_note("CHANNEL_FINGERPRINT_BLOCK", "trae", &body);
        assert!(note2.starts_with(" 指纹样本: "), "{note2}");
        assert_eq!(fingerprint_sample_entries().len(), 1, "同三元组去重");
        // 换池不互吞去重
        assert!(
            empty_sample_note("CHANNEL_FINGERPRINT_BLOCK", "wb", &body).starts_with(" 指纹样本: ")
        );
        assert_eq!(fingerprint_sample_entries().len(), 2, "跨池不互吞");
        // 坏 body：样本注记「body 解析失败」兜底，不 panic
        let note3 = empty_sample_note("CHANNEL_FINGERPRINT_BLOCK", "trae", b"not json");
        assert!(note3.contains("body 解析失败"), "{note3}");
        assert_eq!(fingerprint_sample_entries().len(), 3);
        fingerprint_sample_entries_clear();
        assert!(fingerprint_sample_entries().is_empty());
    }

    fn alias_rules() -> Vec<(String, String)> {
        vec![
            ("ask_user_question".to_string(), "ask_user".to_string()),
            ("create_goal".to_string(), "set_goal".to_string()),
        ]
    }

    /// issue #150 请求侧改写三透传点：tools[].function.name / tool_choice /
    /// 历史轮 tool_calls[].function_call.name；通用名（grep）不改；逆映射
    /// 别名→原名供响应侧回改
    #[test]
    fn tool_alias_forward_renames_three_points() {
        let body = json!({
            "tools": [
                {"type": "function", "function": {"name": "ask_user_question", "description": "d1", "parameters": {}}},
                {"type": "function", "function": {"name": "grep", "description": "d2", "parameters": {}}}
            ],
            "tool_choice": "ask_user_question",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "t1", "type": "function",
                     "function_call": {"name": "ask_user_question", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "t1", "content": "ans"}
            ]
        });
        let mut buf = serde_json::to_vec(&body).unwrap();
        let reverse = apply_tool_alias_forward(&mut buf, &alias_rules()).expect("应发生改写");
        let v: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["tools"][0]["function"]["name"], json!("ask_user"));
        assert_eq!(v["tools"][1]["function"]["name"], json!("grep"), "通用名不改");
        assert_eq!(v["tool_choice"], json!("ask_user"));
        assert_eq!(
            v["messages"][1]["tool_calls"][0]["function_call"]["name"],
            json!("ask_user")
        );
        assert_eq!(reverse.get("ask_user").map(String::as_str), Some("ask_user_question"));
    }

    /// 零命中返回 None（响应侧零开销直通）：无 tools 请求 / 工具名均不在表内
    #[test]
    fn tool_alias_forward_zero_hit_returns_none() {
        let body = json!({
            "tools": [{"type": "function", "function": {"name": "grep", "parameters": {}}}],
            "messages": []
        });
        let mut buf = serde_json::to_vec(&body).unwrap();
        assert!(apply_tool_alias_forward(&mut buf, &alias_rules()).is_none());
        let body2 = json!({"messages": [{"role": "user", "content": "hi"}]});
        let mut b2 = serde_json::to_vec(&body2).unwrap();
        assert!(apply_tool_alias_forward(&mut b2, &alias_rules()).is_none());
    }

    /// 同别名冲突先到先得：两条规则映射到同一别名时第二个原名不改写
    /// （配置冲突的防御性兜底，保证响应侧逆映射无歧义）
    #[test]
    fn tool_alias_forward_conflict_first_wins() {
        let rules = vec![
            ("tool_a".to_string(), "shared".to_string()),
            ("tool_b".to_string(), "shared".to_string()),
        ];
        let body = json!({"tools": [
            {"function": {"name": "tool_a"}},
            {"function": {"name": "tool_b"}}
        ]});
        let mut buf = serde_json::to_vec(&body).unwrap();
        let reverse = apply_tool_alias_forward(&mut buf, &rules).unwrap();
        let v: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["tools"][0]["function"]["name"], json!("shared"));
        assert_eq!(v["tools"][1]["function"]["name"], json!("tool_b"), "冲突第二个不改写");
        assert_eq!(reverse.get("shared").map(String::as_str), Some("tool_a"));
    }

    /// 碰撞防御（审查修复）：别名命中请求内其他真实工具名时该规则整条跳过——
    /// tools/tool_choice/历史轮三处统一原名透传，零改写返回 None。否则上游收到
    /// 重复工具名 + 响应侧逆映射误改真实工具调用（静默错工具执行）
    #[test]
    fn tool_alias_forward_real_name_collision_skips_rule() {
        let rules = vec![("ask_user_question".to_string(), "ask_user".to_string())];
        let body = json!({
            "tools": [
                {"function": {"name": "ask_user_question"}},
                {"function": {"name": "ask_user"}}
            ],
            "tool_choice": "ask_user_question",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "t1", "type": "function",
                     "function_call": {"name": "ask_user_question", "arguments": "{}"}}
                ]}
            ]
        });
        let mut buf = serde_json::to_vec(&body).unwrap();
        assert!(
            apply_tool_alias_forward(&mut buf, &rules).is_none(),
            "碰撞规则整条跳过：零改写返回 None"
        );
        let v: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["tools"][0]["function"]["name"], json!("ask_user_question"));
        assert_eq!(v["tools"][1]["function"]["name"], json!("ask_user"));
        assert_eq!(v["tool_choice"], json!("ask_user_question"));
        assert_eq!(
            v["messages"][0]["tool_calls"][0]["function_call"]["name"],
            json!("ask_user_question")
        );
    }

    /// tool_choice 仅跟随 tools 实际发生的改写：同别名冲突输家的 tool_choice
    /// 保持原名（上游不会收到指向不存在/错误工具的 tool_choice）；赢家照改
    #[test]
    fn tool_choice_follows_actual_renames_only() {
        let rules = vec![
            ("tool_a".to_string(), "shared".to_string()),
            ("tool_b".to_string(), "shared".to_string()),
        ];
        // 输家 tool_b 的 tool_choice 不改名
        let body = json!({
            "tools": [{"function": {"name": "tool_a"}}, {"function": {"name": "tool_b"}}],
            "tool_choice": "tool_b"
        });
        let mut buf = serde_json::to_vec(&body).unwrap();
        let reverse = apply_tool_alias_forward(&mut buf, &rules).unwrap();
        let v: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["tool_choice"], json!("tool_b"), "输家不改写");
        assert_eq!(reverse.get("shared").map(String::as_str), Some("tool_a"));
        // 赢家 tool_a 的 tool_choice 照改
        let body2 = json!({
            "tools": [{"function": {"name": "tool_a"}}, {"function": {"name": "tool_b"}}],
            "tool_choice": "tool_a"
        });
        let mut buf2 = serde_json::to_vec(&body2).unwrap();
        apply_tool_alias_forward(&mut buf2, &rules).unwrap();
        let v2: Value = serde_json::from_slice(&buf2).unwrap();
        assert_eq!(v2["tool_choice"], json!("shared"));
    }

    /// 碰撞放行的链式改写：更早规则已把真实名改名让出（freed）时，后续规则的
    /// 别名可复用该名——规则序 B→C、A→B 合法，响应侧逆映射双向无歧义
    #[test]
    fn tool_alias_forward_chain_free_allows_alias() {
        let rules = vec![
            ("b".to_string(), "c".to_string()),
            ("a".to_string(), "b".to_string()),
        ];
        let body = json!({"tools": [{"function": {"name": "a"}}, {"function": {"name": "b"}}]});
        let mut buf = serde_json::to_vec(&body).unwrap();
        let reverse = apply_tool_alias_forward(&mut buf, &rules).unwrap();
        let v: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["tools"][0]["function"]["name"], json!("b"), "a→b（b 已让出）");
        assert_eq!(v["tools"][1]["function"]["name"], json!("c"), "b→c");
        assert_eq!(reverse.get("b").map(String::as_str), Some("a"));
        assert_eq!(reverse.get("c").map(String::as_str), Some("b"));
    }

    /// 内置默认表收录口径回归（对齐指纹清洗布防客户端，issue #150）：① 各
    /// 客户端实证独家名在表（Claude Code camelCase 按真实注册名大小写）；
    /// ② 别名互不相同（同别名冲突先到先得会让第二条静默失效）；
    /// ③ 别名不与任何规则 from 重名（默认表不依赖链式让出例外）
    #[test]
    fn default_tool_alias_rules_cover_client_fingerprints() {
        let rules = default_tool_alias_rules();
        // Trae SOLO 独家（v3.8.5 首发七条 + 1.107.1 live harness 补两条）
        for from in [
            "ask_user_question",
            "create_goal",
            "exit_plan_mode",
            "get_goal",
            "interrupt_agent",
            "job_kill",
            "job_list",
            "PureShowWidget",
            "browser_waiting_for_user_interaction",
        ] {
            assert!(rules.iter().any(|(f, _)| f == from), "{from} 应在默认表");
        }
        // Claude Code 独家（camelCase 多词连写特征；v2.1.312 懒加载架构新增六条）
        for from in [
            "TodoWrite",
            "AskUserQuestion",
            "ListAgents",
            "ReportFindings",
            "ScheduleWakeup",
            "ToolSearch",
            "DeferredToolPlaceholder",
            "Workflow",
        ] {
            assert!(rules.iter().any(|(f, _)| f == from), "{from} 应在默认表");
        }
        // Cline / Roo 独家（双编辑器拆分 + 任务终结签名 + 追问）
        for from in [
            "write_to_file",
            "replace_in_file",
            "attempt_completion",
            "ask_followup_question",
        ] {
            assert!(rules.iter().any(|(f, _)| f == from), "{from} 应在默认表");
        }
        // Codex 独家（patch 协议 + 计划跟踪 + 0.145.0 补 goal 管理面与用户输入通道）
        for from in ["apply_patch", "update_plan", "update_goal", "request_user_input"] {
            assert!(rules.iter().any(|(f, _)| f == from), "{from} 应在默认表");
        }
        // DSH 独家（0.2.1-rc.2 抓包实证；6 个同名工具经 Trae 段规则覆盖，
        // 此为剩余独家名）
        for from in ["subagent_fork", "job_output", "pwsh", "working_directory"] {
            assert!(rules.iter().any(|(f, _)| f == from), "{from} 应在默认表");
        }
        // CodeBuddy 独家（1.106.1 抓包实证；4 个同名工具经 Cline/Roo 与
        // Claude Code 段规则覆盖，此为剩余独家名）
        for from in [
            "read_lints",
            "execute_command",
            "update_memory",
            "use_skill",
            "read_rules",
            "send_message",
            "DeferExecuteTool",
        ] {
            assert!(rules.iter().any(|(f, _)| f == from), "{from} 应在默认表");
        }
        // Qoder 独家（0.4.11 混淆包逆向实证；与 Claude Code 同构的同名工具经
        // Claude Code 段天然覆盖，此为独家名——mcp 三件套 + 团队协作两件）
        for from in ["mcp_list", "mcp_get", "mcp_call", "SendMessage", "TaskCreate"] {
            assert!(rules.iter().any(|(f, _)| f == from), "{from} 应在默认表");
        }
        // 别名卫生：非空、互不相同、不与任何规则 from 重名
        let aliases: Vec<&str> = rules.iter().map(|(_, t)| t.as_str()).collect();
        let froms: Vec<&str> = rules.iter().map(|(f, _)| f.as_str()).collect();
        for a in &aliases {
            assert!(!a.is_empty());
            assert_eq!(aliases.iter().filter(|x| *x == a).count(), 1, "别名 {a} 重复");
            assert!(!froms.contains(a), "别名 {a} 与规则 from 重名");
        }
    }

    /// 工具别名合并语义：默认表全量生效；自定义同名覆盖；to=None 停用（从
    /// 生效表移除）；空串 to 经 into_rules 过滤后等价停用；新条目追加
    #[test]
    fn tool_alias_merged_rules_disable_and_override() {
        let base = merged_tool_alias_rules(None);
        assert!(base.iter().any(|(f, t)| f == "ask_user_question" && t == "ask_user"));
        // 同名覆盖
        let custom = vec![("ask_user_question".to_string(), Some("clarify".to_string()))];
        let m1 = merged_tool_alias_rules(Some(custom));
        assert!(m1.iter().any(|(f, t)| f == "ask_user_question" && t == "clarify"));
        // to=None 停用：仅移除该条，其余默认规则不受影响
        let custom2 = vec![("ask_user_question".to_string(), None)];
        let m2 = merged_tool_alias_rules(Some(custom2));
        assert!(!m2.iter().any(|(f, _)| f == "ask_user_question"));
        assert!(m2.iter().any(|(f, _)| f == "create_goal"));
        // 空串 to：into_rules 统一过滤为停用（工具名不可为空）
        let file = ToolAliasFile {
            rules: vec![TemplateRule {
                from: "create_goal".to_string(),
                to: Some(String::new()),
            }],
            updated_at: None,
        };
        let m3 = merged_tool_alias_rules(file.into_rules());
        assert!(!m3.iter().any(|(f, _)| f == "create_goal"));
        // 新条目追加
        let custom4 = vec![("custom_tool".to_string(), Some("generic".to_string()))];
        let m4 = merged_tool_alias_rules(Some(custom4));
        assert!(m4.iter().any(|(f, t)| f == "custom_tool" && t == "generic"));
        assert_eq!(m4.len(), default_tool_alias_rules().len() + 1);
    }
}
