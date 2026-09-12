/**
 * 全局 API 管理 · 生态接入（统一版，unified-api-gateway-design §5.2/§7）
 * 只注册统一网关条目：沿用 Trae 侧注册调用与 provider id（aiwork-gateway-*，
 * side='trae'），Claude Code 与 Codex 各注册一次；默认模型取网关设置。
 * 历史 Buddy 条目（aiwork-wb-gateway-*）不主动删除，提示可手动清理。
 */
import { useEffect, useState } from 'react';
import { Plug } from 'lucide-react';
import { api } from '../../lib/tauri';
import { withMinDelay } from '../../lib/delay';
import type { GatewaySettings } from '../../types';

export default function EcoAccess() {
  const [ccBusy, setCcBusy] = useState<'claude' | 'codex' | null>(null);
  const [ecoNote, setEcoNote] = useState('');
  const [gwModel, setGwModel] = useState<string | undefined>(undefined);

  // 默认模型取网关设置（§7：与统一网关条目语义一致）；读取失败时不传，后端回退内置默认
  useEffect(() => {
    api.apiServer
      .gatewaySettingsGet()
      .then((gw: GatewaySettings) => setGwModel(gw.default_model))
      .catch(() => {
        /* 后端回退默认模型 */
      });
  }, []);

  const registerCcSwitch = async (appType: 'claude' | 'codex') => {
    if (ccBusy) return;
    setCcBusy(appType);
    setEcoNote('');
    try {
      const msg = await withMinDelay(
        api.apiServer.ccSwitchRegister(appType, 'trae', undefined, gwModel),
      );
      setEcoNote(`✓ ${msg}`);
    } catch (e) {
      setEcoNote(`✗ CC Switch 注册失败：${String(e).slice(0, 160)}`);
    } finally {
      setCcBusy(null);
    }
  };

  return (
    <div className="card p-4">
      <div className="mb-2 flex items-center gap-2">
        <Plug size={16} className="text-emerald-500" />
        <span className="text-sm font-medium text-slate-800 dark:text-zinc-100">生态接入</span>
      </div>
      <div className="flex flex-wrap items-center gap-2">
        <button
          className="btn-outline flex items-center gap-1 !py-1.5 text-xs"
          onClick={() => void registerCcSwitch('claude')}
          disabled={ccBusy !== null}
          title="把统一网关 Anthropic 端点（/v1/messages）注册进 CC Switch（统一条目「AI Work 助手网关」），由 CC Switch 负责切换"
        >
          {ccBusy === 'claude' ? '注册中…' : '注册到 CC Switch（Claude Code）'}
        </button>
        <button
          className="btn-outline flex items-center gap-1 !py-1.5 text-xs"
          onClick={() => void registerCcSwitch('codex')}
          disabled={ccBusy !== null}
          title="把统一网关 Responses 端点（/v1/responses）注册进 CC Switch（统一条目「AI Work 助手网关」），由 CC Switch 负责切换"
        >
          {ccBusy === 'codex' ? '注册中…' : '注册到 CC Switch（Codex）'}
        </button>
      </div>
      {ecoNote && (
        <p className="mt-2 break-all text-[11px] leading-4 text-slate-500 dark:text-zinc-400">
          {ecoNote}
        </p>
      )}
      <p className="mt-2 text-[11px] text-slate-400 dark:text-zinc-500">
        统一注册单条目「AI Work 助手网关」（默认模型 {gwModel ?? 'glm-5.3'}）：CC Switch
        注册会先整库备份至 ~/.cc-switch/backups/，仅写入本网关条目、不改其它 provider；
        写入后需重启 CC Switch 生效。历史 Buddy 条目（aiwork-wb-gateway-*）不再使用，可手动在
        CC Switch 中清理。
      </p>
    </div>
  );
}
