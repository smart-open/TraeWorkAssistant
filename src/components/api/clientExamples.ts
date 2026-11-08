/**
 * F-83（issue #174）客户端配置示例纯函数生成器（docker 版适配）。
 * 客户端集合：OpenAI 兼容 / Claude Code / Codex / Cline / Continue / Roo Code。
 * 消费方：InterfaceConfig.tsx（客户端 Tab 示例）、ApiService.tsx / GatewayHelpModal.tsx（模型目录逐行复制）。
 * docker 版为 Web 单端口架构：接口地址跟随当前访问 origin（gatewayBaseUrl()），
 * 无桌面版固定 127.0.0.1:port 与局域网网卡枚举（LanIfaceIp 不存在于本分支）。
 */

export type ClientExampleKey = 'openai' | 'claude' | 'codex' | 'cline' | 'continue' | 'roo';

export const CLIENT_EXAMPLE_TABS: { key: ClientExampleKey; label: string }[] = [
  { key: 'openai', label: 'OpenAI 兼容' },
  { key: 'claude', label: 'Claude Code' },
  { key: 'codex', label: 'Codex' },
  { key: 'cline', label: 'Cline' },
  { key: 'continue', label: 'Continue' },
  { key: 'roo', label: 'Roo Code' },
];

/** 模型目录逐行复制片段：base_url / API Key / 模型名三字段一次到位（base 为网关 origin） */
export function buildModelCopySnippet(base: string, model: string): string {
  return [
    `接口地址: ${base}/v1`,
    'API Key:  <在「API Keys 管理」创建并复制完整 Key（ck_ 前缀）>',
    `模型 ID:  ${model}`,
  ].join('\n');
}

/** 各客户端配置片段（base 为网关 origin，model 为默认模型 ID） */
export function buildClientExamples(base: string, model: string): Record<ClientExampleKey, string> {
  const baseV1 = `${base}/v1`;

  const openai = [
    '# 客户端配置示例（OpenAI 兼容格式）',
    `接口地址: ${baseV1}`,
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
    '# bash / zsh',
    `export ANTHROPIC_BASE_URL="${base}"`,
    'export ANTHROPIC_AUTH_TOKEN="<API Key>"',
    '',
    '# ~/.claude/settings.json（持久化形态）',
    '{',
    '  "env": {',
    `    "ANTHROPIC_BASE_URL": "${base}",`,
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
