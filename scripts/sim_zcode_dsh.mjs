#!/usr/bin/env node
/**
 * issue #57 客户端模拟测试脚本：ZCode / DSH 指纹清洗端到端验证
 *
 * 背景：
 *   上游渠道风控按请求指纹拦截（11128 Illegal API invocation / 影子风控空响应），
 *   网关已布防指纹清洗（strip_cc + 模板映射热更新）+ 11128 强制清洗重试 +
 *   空完成哨兵换号。ZCode / DSH 的真实身份句无公开资料，本脚本用于：
 *
 *   1. 以两客户端的请求形态（OpenAI 兼容 /v1/chat/completions）打本机网关，
 *      验证「指纹句进站 → 清洗 → 正常完成」链路端到端可用；
 *   2. probe 模式：单请求携带全部已知 harness 身份句（Claude Code / Codex /
 *      OpenCode / TraeCode / Cline / Roo / Gemini CLI），验证清洗生效——
 *      网关调度日志应出现「11128 强制清洗重试；模板命中: …」或命中计数；
 *   3. --system-file 注入真实抓包的 system prompt（ZCode/DSH 迭代通道：
 *      跑真实客户端抓包 → 注入本脚本复现 → 未布防则写 wb_template_map.json 热更新）。
 *
 * 用法（Windows PowerShell）：
 *   node scripts/sim_zcode_dsh.mjs --base-url http://127.0.0.1:8899 --api-key sk-xxx
 *   node scripts/sim_zcode_dsh.mjs ... --case probe
 *   node scripts/sim_zcode_dsh.mjs ... --case zcode --system-file zcode_system.txt
 *
 * 判定：
 *   PASS  HTTP 200 且完成内容非空
 *   WARN  HTTP 200 但内容为空（疑似影子风控 → 查网关日志「空完成 → 换号重试」）
 *   FAIL  11128 / 其他 4xx5xx（查日志「11128 强制清洗重试；模板命中: …」）
 *
 * 仅用 Node.js 内置能力（Node 18+ global fetch），无第三方依赖。
 */

import { readFileSync } from "node:fs";

// ---------------------------------------------------------------------------
// 模拟身份句（关键：已知句为逐字实证，speculative 句仅为占位模拟，非真实抓包）
// ---------------------------------------------------------------------------

// TraeCode harness 首句（日志实证 pool=trae 请求原文，已布防）
const TRAE_CODE_SENTENCE =
  "You are an interactive agent in TraeCode that helps the USER " +
  "with software engineering tasks.";

// DSH 桥（dsh-connect-trae）：桥接 Trae 模型进 DSH，请求形态大概率复用
// TraeCode harness —— 用实证句模拟；真实 DSH 专属句若有，用 --system-file 注入
const DSH_SYSTEM =
  TRAE_CODE_SENTENCE + "\n\n" +
  "# Environment\n" +
  "- Working directory: d:\\demo-project\n" +
  "- Platform: win32\n";

// ZCode（Z.ai 桌面 harness，GLM 系）：身份句无公开资料，此处为 speculative
// 占位（含 ZCode 字样便于日志反查）；真实句请抓包后经 --system-file 注入
const ZCODE_SYSTEM =
  "You are ZCode Agent, the default agent in ZCode. " +
  "You help the user with software engineering tasks.\n\n" +
  "# Workspace\n" +
  "- Workspace root: d:\\demo-project\n";

// probe 用：全部已知 harness 身份句（default_template_map 已布防的逐字实证句）
const PROBE_SYSTEM = [
  "You are Claude Code, Anthropic's official CLI for Claude.",
  "Main branch (you will usually use this with PRs)",
  "You are Codex, based on GPT-5.",
  "You are Codex, an agent based on GPT-5.",
  "You are Codex, a coding agent based on GPT-5.",
  "You are a coding agent running in the Codex CLI.",
  TRAE_CODE_SENTENCE,
  "You are Cline, a highly skilled software engineer with extensive knowledge",
  "You are Roo, a highly skilled software engineer with extensive knowledge",
  "You are an interactive CLI agent specializing in software engineering tasks.",
  "You are opencode, an interactive CLI tool that helps users " +
    "with software engineering tasks.",
  "You are OpenCode, You and the user share the same workspace.",
  "You are OpenCode, the best coding agent on the planet.",
  "You are opencode, an interactive CLI agent specializing in " +
    "software engineering tasks.",
  "You are operating as and within the OpenCode CLI, a terminal-based " +
    "agentic coding assistant built by OpenAI.",
  "cc_user_id=12345; x-anthropic-beta: prompt-caching",
].join("\n");

// 工具描述指纹（harness 常在 tools[].function.description 注入身份痕迹）
const FINGERPRINT_TOOL = {
  type: "function",
  function: {
    name: "run_command",
    description:
      "Run a shell command. " +
      "You are Claude Code, Anthropic's official CLI for Claude.",
    parameters: {
      type: "object",
      properties: { cmd: { type: "string" } },
      required: ["cmd"],
    },
  },
};

function buildBody(system, model, stream, user = "1+1=?只回数字") {
  return {
    model,
    stream,
    max_tokens: 1024,
    messages: [
      { role: "system", content: system },
      { role: "user", content: user },
    ],
    tools: [FINGERPRINT_TOOL],
    tool_choice: "auto",
  };
}

// ---------------------------------------------------------------------------
// HTTP 层（流式按 SSE 行解析，非流式直接 JSON）
// ---------------------------------------------------------------------------

async function post(baseUrl, apiKey, body, timeoutMs = 120_000) {
  const t0 = Date.now();
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const resp = await fetch(baseUrl.replace(/\/+$/, "") + "/v1/chat/completions", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${apiKey}`,
      },
      body: JSON.stringify(body),
      signal: controller.signal,
    });
    if (!resp.ok) {
      const detail = (await resp.text()).slice(0, 500);
      return { status: resp.status, text: "", err: detail };
    }
    const text = body.stream
      ? await readSse(resp)
      : extractNonstream(await resp.text());
    return { status: resp.status, text, err: null };
  } catch (e) {
    return { status: null, text: "", err: e.name === "AbortError" ? "timeout" : String(e) };
  } finally {
    clearTimeout(timer);
  }
}

/** 收集 SSE 流中的可见文本（delta.content），校验 [DONE] 与错误帧 */
async function readSse(resp) {
  const chunks = [];
  let done = false;
  const decoder = new TextDecoder();
  let buf = "";
  for await (const raw of resp.body) {
    buf += decoder.decode(raw, { stream: true });
    let idx;
    while ((idx = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, idx).replace(/\r$/, "").trim();
      buf = buf.slice(idx + 1);
      if (!line.startsWith("data:")) continue;
      const payload = line.slice(5).trim();
      if (payload === "[DONE]") { done = true; continue; }
      let ev;
      try { ev = JSON.parse(payload); } catch { continue; }
      if (ev.error) {
        return `<<error_frame: ${JSON.stringify(ev.error).slice(0, 300)}>>`;
      }
      for (const ch of ev.choices ?? []) {
        const c = ch.delta?.content;
        if (typeof c === "string") chunks.push(c);
      }
    }
  }
  let out = chunks.join("");
  if (!done && out) out += "\n<<warn: 未收到 [DONE] 收尾帧>>";
  return out;
}

function extractNonstream(raw) {
  try {
    const r = JSON.parse(raw);
    if (r.error) return `<<error: ${JSON.stringify(r.error).slice(0, 300)}>>`;
    return r.choices?.[0]?.message?.content ?? "";
  } catch {
    return raw.slice(0, 300);
  }
}

function verdict(status, text) {
  if (status === 200 && text && !text.includes("<<error") && !text.includes("<<warn")) return "PASS";
  if (status === 200) return "WARN"; // 空完成 / 哨兵错误帧：疑似影子风控
  return "FAIL";
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

const CASES = {
  // ZCode：GLM 系模型（Trae 池），speculative 身份句 + 工具描述指纹
  zcode: { model: "glm-5.3", system: ZCODE_SYSTEM, streams: [true, false] },
  // DSH：桥装客户端，TraeCode 实证句模拟 + 工具描述指纹
  dsh: { model: "deepseek-v4.1-flash", system: DSH_SYSTEM, streams: [true, false] },
  // probe：全部已知身份句一次打满（验证清洗 + 命中计数日志）
  probe: { model: "glm-5.3", system: PROBE_SYSTEM, streams: [true] },
};

// ---------------------------------------------------------------------------
// 参数解析（零依赖）
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const args = {
    "base-url": "http://127.0.0.1:8899",
    "api-key": "sk-test",
    case: "all",
    model: null,
    "system-file": null,
    "stream-only": false,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--stream-only") args["stream-only"] = true;
    else if (a.startsWith("--")) {
      const key = a.slice(2);
      if (!(key in args)) {
        console.error(`未知参数: ${a}`);
        process.exit(2);
      }
      if (typeof args[key] === "boolean") args[key] = true;
      else args[key] = argv[++i];
    }
  }
  if (!["all", "zcode", "dsh", "probe"].includes(args.case)) {
    console.error("--case 取值: all | zcode | dsh | probe");
    process.exit(2);
  }
  return args;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  let customSystem = null;
  if (args["system-file"]) {
    customSystem = readFileSync(args["system-file"], "utf8");
  }

  const names = args.case === "all" ? Object.keys(CASES) : [args.case];
  const results = [];
  for (const name of names) {
    const kase = CASES[name];
    const model = args.model ?? kase.model;
    const system = customSystem ?? kase.system;
    const streams = args["stream-only"] ? [true] : kase.streams;
    for (const stream of streams) {
      const tag = `${name} [${stream ? "stream" : "nonstream"}] model=${model}`;
      const t0 = Date.now();
      const { status, text, err } = await post(
        args["base-url"], args["api-key"], buildBody(system, model, stream),
      );
      const elapsed = (Date.now() - t0) / 1000;
      results.push({ tag, v: verdict(status, text), status, elapsed, text, err });
    }
  }

  const line = "=".repeat(78);
  console.log(line);
  console.log(`issue #57 客户端模拟测试  base=${args["base-url"]}`);
  console.log(line);
  for (const r of results) {
    console.log(`[${r.v}] ${r.tag}  http=${r.status}  ${r.elapsed.toFixed(1)}s`);
    if (r.err) console.log(`      err: ${r.err}`);
    const snippet = (r.text ?? "").trim().replace(/\n/g, " ").slice(0, 120);
    console.log(`      resp: ${snippet || "(空)"}`);
  }
  console.log("-".repeat(78));
  console.log("判定说明：PASS=正常完成  WARN=空完成/哨兵帧(疑似影子风控)  FAIL=11128等错误");
  console.log("排障通道：网关调度日志查「11128 强制清洗重试；模板命中: …」/「空完成 → 换号重试」");
  console.log("ZCode/DSH 真实身份句迭代：抓包 system prompt → --system-file 复现 →");
  console.log("未布防句写入 wb_template_map.json 热更新（无须发版）");
  const failed = results.filter((r) => r.v === "FAIL").length;
  const warned = results.filter((r) => r.v === "WARN").length;
  console.log(`结果：${results.length - failed - warned} PASS / ${warned} WARN / ${failed} FAIL`);
  process.exit(failed ? 1 : 0);
}

main().catch((e) => {
  console.error("脚本异常:", e);
  process.exit(1);
});
