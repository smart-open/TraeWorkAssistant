/**
 * F-83（issue #174）：客户端配置示例生成器（纯函数，与 UI 解耦）
 * 各客户端「复制即可用」片段统一在此构造：真实端口 + 默认模型 + Key 占位（ck_ 前缀提示）。
 * 客户端集合：OpenAI 兼容 / Claude Code / Codex / Cline / Continue / Roo Code。
 * 消费方：InterfaceConfig.tsx（客户端 Tab 示例）、ApiService.tsx / GatewayHelpModal.tsx（模型目录逐行复制）。
 */
import type { LanIfaceIp } from '../../types';

export type ClientExampleKey = 'openai' | 'claude' | 'codex' | 'cline' | 'continue' | 'roo';

export const CLIENT_EXAMPLE_TABS: { key: ClientExampleKey; label: string }[] = [
  { key: 'openai', label: 'OpenAI 兼容' },
  { key: 'claude', label: 'Claude Code' },
  { key: 'codex', label: 'Codex' },
  { key: 'cline', label: 'Cline' },
  { key: 'continue', label: 'Continue' },
  { key: 'roo', label: 'Roo Code' },
];

/** 模型目录逐行复制片段：base_url / API Key / 模型名三字段一次到位 */
export function buildModelCopySnippet(port: number, model: string): string {
  return [
    `接口地址: http://127.0.0.1:${port}/v1`,
    'API Key:  <在「API Keys 管理」创建并复制完整 Key（ck_ 前缀）>',
    `模型 ID:  ${model}`,
  ].join('\n');
}

/** 各客户端配置片段（port 为真实网关端口，model 为默认模型 ID） */
export function buildClientExamples(
  port: number,
  model: string,
  lanIps: LanIfaceIp[],
): Record<ClientExampleKey, string> {
  const baseV1 = `http://127.0.0.1:${port}/v1`;
  const lanLines = lanIps
    .map((e) => `# 局域网设备接入: http://${e.ip}:${port}/v1  # ${e.name}`)
    .join('\n');

  const openai = [
    '# 客户端配置示例（OpenAI 兼容格式）',
    `接口地址: ${baseV1}`,
    ...(lanLines ? [lanLines] : []),
    'API Key:  <在「API Keys 管理」创建并复制完整 Key（ck_ 前缀）>',
    `模型 ID:  ${model}（统一目录内任一模型均可，请求按模型 ID 匹配资源池）`,
    '',
    '# cURL 测试（替换 <API Key>）',
    `curl -X POST ${baseV1}/chat/completions \\`,
    '  -H "Content-Type: application/json" \\',
    '  -H "Authorization: Bearer <API Key>" \\',
    "  -d '{",
    `    "model": "${model}",`,
    '    "messages": [{"role": "user", "content": "你好"}],',
    '    "stream": true',
    "  }'",
  ].join('\n');

  const claude = [
    '# Claude Code 直连（网关 Anthropic 兼容端点 /v1/messages）',
    '# <API Key> =「API Keys 管理」创建的完整 Key（ck_ 前缀）',
    '',
    '# PowerShell（当前会话）',
    `$env:ANTHROPIC_BASE_URL = "http://127.0.0.1:${port}"`,
    '$env:ANTHROPIC_AUTH_TOKEN = "<API Key>"',
    '',
    '# PowerShell（永久，重开终端生效）',
    `setx ANTHROPIC_BASE_URL "http://127.0.0.1:${port}"`,
    'setx ANTHROPIC_AUTH_TOKEN "<API Key>"',
    '',
    '# bash / zsh',
    `export ANTHROPIC_BASE_URL="http://127.0.0.1:${port}"`,
    'export ANTHROPIC_AUTH_TOKEN="<API Key>"',
    '',
    '# ~/.claude/settings.json（持久化形态）',
    '{',
    '  "env": {',
    `    "ANTHROPIC_BASE_URL": "http://127.0.0.1:${port}",`,
    '    "ANTHROPIC_AUTH_TOKEN": "<API Key>"',
    '  }',
    '}',
    '',
    `# 可选：固定模型  ANTHROPIC_MODEL="${model}"`,
  ].join('\n');

  const codex = [
    '# Codex CLI 直连（~/.codex/config.toml）',
    'model_provider = "aiwork"',
    `model = "${model}"`,
    '',
    '[model_providers.aiwork]',
    'name = "AI Work 助手网关"',
    `base_url = "${baseV1}"`,
    'wire_api = "responses"   # 网关已支持 /v1/responses；如遇兼容问题可改 "chat"',
    'env_key = "AIWORK_API_KEY"',
    '',
    '# API Key 环境变量（<API Key> = ck_ 前缀完整值）',
    '# PowerShell（永久）',
    'setx AIWORK_API_KEY "<API Key>"',
    '# bash / zsh',
    'export AIWORK_API_KEY="<API Key>"',
  ].join('\n');

  const cline = [
    '# Cline（VS Code 插件）直连步骤',
    '# 1. 侧栏 Cline 图标 → 设置（齿轮）→ API Provider 选「OpenAI Compatible」',
    '# 2. 填入以下三项（<API Key> = ck_ 前缀完整值）：',
    `Base URL: ${baseV1}`,
    'API Key:  <API Key>',
    `Model ID: ${model}`,
  ].join('\n');

  const cont = [
    '# Continue（VS Code / JetBrains 插件）直连（~/.continue/config.yaml）',
    'models:',
    '  - name: AI Work 助手网关',
    '    provider: openai',
    `    model: ${model}`,
    `    apiBase: ${baseV1}`,
    '    apiKey: <API Key>   # ck_ 前缀完整值',
    '    roles: [chat, edit, apply]',
  ].join('\n');

  const roo = [
    '# Roo Code（VS Code 插件）直连步骤',
    '# 1. 设置 → API Provider 选「OpenAI Compatible」',
    '# 2. 填入以下三项（<API Key> = ck_ 前缀完整值）：',
    `Base URL: ${baseV1}`,
    'API Key:  <API Key>',
    `Model ID: ${model}`,
  ].join('\n');

  return { openai, claude, codex, cline, continue: cont, roo };
}
