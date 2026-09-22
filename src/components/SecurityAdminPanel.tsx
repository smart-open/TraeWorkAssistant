import { useEffect, useState } from 'react';
import { Save, BellRing, Send, ShieldCheck, KeyRound } from 'lucide-react';
import { useAppStore } from '../store';
import { withMinDelay } from '../lib/delay';
import { copyText } from '../lib/clipboard';
import { api } from '../lib/tauri';
import type { NotifyConfig as NotifyConfigType, NotifyResult, IpAllowlistConfig, AdminTokenView } from '../types';

/**
 * 安全与管理面板（系统设置弹框 · 安全与管理 Tab）：
 * 全局性配置从 Trae · 环境配置页迁入（业务配置仍留在各平台环境配置页）——
 * 通知渠道（Bark / Server酱 / Webhook）、IP 允许列表、管理员令牌。
 * 三块均为独立 kv / 即时生效，各自保存，不参与其他表单的保存流程。
 */
export default function SecurityAdminPanel() {
  const toast = useAppStore((s) => s.pushToast);

  // ---- 通知渠道（T11）----
  const [notifyForm, setNotifyForm] = useState<NotifyConfigType | null>(null);
  const [notifySaving, setNotifySaving] = useState(false);
  const [notifyTesting, setNotifyTesting] = useState(false);

  useEffect(() => {
    api.notify
      .getConfig()
      .then(setNotifyForm)
      .catch(() => setNotifyForm(null));
  }, []);

  const saveNotify = async () => {
    if (!notifyForm) return;
    setNotifySaving(true);
    try {
      const saved = await withMinDelay(api.notify.setConfig(notifyForm));
      setNotifyForm(saved);
      toast('success', '通知配置已保存');
    } catch (e) {
      toast('error', e instanceof Error ? e.message : '通知配置保存失败');
    } finally {
      setNotifySaving(false);
    }
  };

  const testNotify = async () => {
    if (!notifyForm) return;
    setNotifyTesting(true);
    try {
      const res = await withMinDelay(api.notify.test(notifyForm));
      if (res.sent) {
        toast('success', '测试通知已发送，请查收');
      } else {
        // 汇总各渠道失败原因；全部 null 视为未配置
        const fails = [res.bark, res.serverchan, res.webhook].filter(
          (x): x is string => !!x && x !== 'ok',
        );
        toast('error', fails.length ? `发送失败：${fails.join('；')}` : res.reason ?? '未配置任何通知渠道');
      }
    } catch (e) {
      toast('error', e instanceof Error ? e.message : '测试发送失败');
    } finally {
      setNotifyTesting(false);
    }
  };

  // ---- IP 允许列表（T12a）----
  const [ipForm, setIpForm] = useState<IpAllowlistConfig | null>(null);
  const [ipSaving, setIpSaving] = useState(false);
  const [ipCidrsText, setIpCidrsText] = useState('');

  useEffect(() => {
    api.ipAllowlist
      .getConfig()
      .then((c) => {
        setIpForm(c);
        setIpCidrsText(c.cidrs.join('\n'));
      })
      .catch(() => setIpForm(null));
  }, []);

  const saveIpAllowlist = async () => {
    if (!ipForm) return;
    // 文本域按行/逗号拆分，trim + 去空（服务端还会去重并逐条校验）
    const cidrs = ipCidrsText
      .split(/[\n,]/)
      .map((s) => s.trim())
      .filter(Boolean);
    setIpSaving(true);
    try {
      const saved = await withMinDelay(api.ipAllowlist.setConfig({ ...ipForm, cidrs }));
      setIpForm(saved);
      setIpCidrsText(saved.cidrs.join('\n'));
      toast('success', 'IP 允许列表已保存并即时生效');
    } catch (e) {
      toast('error', e instanceof Error ? e.message : 'IP 允许列表保存失败');
    } finally {
      setIpSaving(false);
    }
  };

  // ---- 管理员令牌（T12b）----
  const [tokens, setTokens] = useState<AdminTokenView[] | null>(null);
  const [tokenLabel, setTokenLabel] = useState('');
  const [tokenBusy, setTokenBusy] = useState(false);
  const [newTokenPlain, setNewTokenPlain] = useState<string | null>(null);

  const loadTokens = () => {
    api.adminTokens
      .list()
      .then(setTokens)
      .catch(() => setTokens(null));
  };
  useEffect(() => {
    loadTokens();
  }, []);

  const createToken = async () => {
    if (!tokenLabel.trim()) {
      toast('error', '请填写备注名称');
      return;
    }
    setTokenBusy(true);
    try {
      const entry = await withMinDelay(api.adminTokens.create(tokenLabel.trim()));
      setTokenLabel('');
      setNewTokenPlain(entry.token);
      loadTokens();
      toast('success', '令牌已创建，请立即复制保存');
    } catch (e) {
      toast('error', e instanceof Error ? e.message : '创建失败');
    } finally {
      setTokenBusy(false);
    }
  };

  const revokeToken = async (id: string) => {
    setTokenBusy(true);
    try {
      await withMinDelay(api.adminTokens.revoke(id));
      loadTokens();
      toast('success', '令牌已吊销，该令牌会话即刻失效');
    } catch (e) {
      toast('error', e instanceof Error ? e.message : '吊销失败');
    } finally {
      setTokenBusy(false);
    }
  };

  const copyNewToken = async () => {
    if (!newTokenPlain) return;
    const ok = await copyText(newTokenPlain);
    toast(ok ? 'success' : 'error', ok ? '已复制到剪贴板' : '复制失败，请手动选择复制');
  };

  return (
    <div className="flex flex-col gap-4 text-sm">
      {/* 通知渠道：独立配置即时保存 */}
      {notifyForm && (
        <section className="card p-4">
          <div className="mb-1 flex flex-wrap items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <BellRing size={16} className="text-brand-500" />
              <h3 className="font-medium">通知渠道</h3>
            </div>
            <div className="flex items-center gap-2">
              <button onClick={testNotify} disabled={notifyTesting} className="btn-outline">
                <Send size={15} /> {notifyTesting ? '发送中…' : '发送测试'}
              </button>
              <button onClick={saveNotify} disabled={notifySaving} className="btn-primary">
                <Save size={15} /> {notifySaving ? '保存中…' : '保存'}
              </button>
            </div>
          </div>
          <p className="mb-3 text-xs text-slate-400">
            签到完成 / 调度任务失败时推送到手机（Bark / Server酱）或自建 Webhook，未配置的渠道自动跳过。
          </p>
          <div className="grid gap-4 text-sm md:grid-cols-2">
            <div className="space-y-3">
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={notifyForm.enabled}
                  onChange={(e) => setNotifyForm({ ...notifyForm, enabled: e.target.checked })}
                />
                启用通知推送（总开关）
              </label>
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={notifyForm.on_checkin_done}
                  onChange={(e) => setNotifyForm({ ...notifyForm, on_checkin_done: e.target.checked })}
                />
                签到完成时通知
              </label>
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={notifyForm.on_task_failed}
                  onChange={(e) => setNotifyForm({ ...notifyForm, on_task_failed: e.target.checked })}
                />
                调度任务失败时通知
              </label>
            </div>
            <div className="space-y-3">
              <div>
                <label className="label">Bark 推送地址</label>
                <input
                  value={notifyForm.bark_url ?? ''}
                  onChange={(e) => setNotifyForm({ ...notifyForm, bark_url: e.target.value || null })}
                  placeholder="https://api.day.app/你的Key"
                  className="input w-full"
                />
              </div>
              <div>
                <label className="label">Server酱 SendKey</label>
                <input
                  value={notifyForm.serverchan_sendkey ?? ''}
                  onChange={(e) =>
                    setNotifyForm({ ...notifyForm, serverchan_sendkey: e.target.value || null })
                  }
                  placeholder="SCT…（sct.ftqq.com 获取）"
                  className="input w-full"
                />
              </div>
              <div>
                <label className="label">通用 Webhook 地址</label>
                <input
                  value={notifyForm.webhook_url ?? ''}
                  onChange={(e) => setNotifyForm({ ...notifyForm, webhook_url: e.target.value || null })}
                  placeholder="https://…（POST JSON）"
                  className="input w-full"
                />
              </div>
            </div>
          </div>
        </section>
      )}

      {/* IP 允许列表：独立配置即时保存 */}
      {ipForm && (
        <section className="card p-4">
          <div className="mb-1 flex flex-wrap items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <ShieldCheck size={16} className="text-brand-500" />
              <h3 className="font-medium">IP 允许列表</h3>
            </div>
            <button onClick={saveIpAllowlist} disabled={ipSaving} className="btn-primary">
              <Save size={15} /> {ipSaving ? '保存中…' : '保存'}
            </button>
          </div>
          <p className="mb-3 text-xs text-slate-400">
            启用后仅允许列表内 IP 访问网关与管理面（/health 探活除外），保存后立即生效——
            请确保当前访问 IP 已在列表内。回环地址（127.0.0.1 / ::1）始终放行，防止自锁。
          </p>
          <div className="grid gap-4 text-sm md:grid-cols-2">
            <div className="space-y-3">
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={ipForm.enabled}
                  onChange={(e) => setIpForm({ ...ipForm, enabled: e.target.checked })}
                />
                启用 IP 允许列表（总开关；列表为空时同样放行）
              </label>
              <label className="flex items-center gap-2">
                <input
                  type="checkbox"
                  checked={ipForm.trust_proxy}
                  onChange={(e) => setIpForm({ ...ipForm, trust_proxy: e.target.checked })}
                />
                位于反向代理之后（取 X-Real-IP / X-Forwarded-For）
              </label>
              <p className="text-xs text-slate-400">
                直连部署请保持关闭，否则客户端可伪造头绕过限制。
              </p>
            </div>
            <div>
              <label className="label">允许的 CIDR / IP（每行一个）</label>
              <textarea
                value={ipCidrsText}
                onChange={(e) => setIpCidrsText(e.target.value)}
                rows={5}
                placeholder={'192.168.1.0/24\n10.0.0.5\n2001:db8::/32'}
                className="input w-full font-mono text-xs"
              />
            </div>
          </div>
        </section>
      )}

      {/* 管理员令牌：操作即时生效 */}
      {tokens && (
        <section className="card p-4">
          <div className="mb-1 flex flex-wrap items-center justify-between gap-2">
            <div className="flex items-center gap-2">
              <KeyRound size={16} className="text-brand-500" />
              <h3 className="font-medium">管理员令牌</h3>
            </div>
            <div className="flex items-center gap-2">
              <input
                value={tokenLabel}
                onChange={(e) => setTokenLabel(e.target.value)}
                placeholder="备注名称（如：运维同事）"
                className="input w-48"
              />
              <button onClick={createToken} disabled={tokenBusy} className="btn-primary">
                <KeyRound size={15} /> 创建
              </button>
            </div>
          </div>
          <p className="mb-3 text-xs text-slate-400">
            主 token（环境变量 AIWORK_ADMIN_TOKEN / conf/admin_token）始终有效且不可吊销；
            以下为可分发的附加令牌，持有者可用其登录管理面，吊销后即刻失效。
          </p>
          {newTokenPlain && (
            <div className="mb-3 flex flex-wrap items-center gap-2 rounded-lg border border-amber-300 bg-amber-50 p-3 dark:border-amber-700 dark:bg-amber-900/20">
              <span className="text-xs font-medium text-amber-600 dark:text-amber-400">新令牌（仅此一次展示）：</span>
              <code className="min-w-0 flex-1 break-all font-mono text-xs">{newTokenPlain}</code>
              <button onClick={copyNewToken} className="btn-outline text-xs">
                复制
              </button>
              <button onClick={() => setNewTokenPlain(null)} className="btn-outline text-xs">
                我已保存
              </button>
            </div>
          )}
          {tokens.length === 0 ? (
            <div className="text-sm text-slate-400">暂无附加令牌</div>
          ) : (
            <div className="space-y-2">
              {tokens.map((t) => (
                <div
                  key={t.id}
                  className="flex flex-wrap items-center gap-3 rounded-lg bg-slate-50 px-3 py-2 text-sm dark:bg-zinc-900"
                >
                  <span className="font-medium">{t.label}</span>
                  <code className="font-mono text-xs text-slate-500">{t.token_masked}</code>
                  <span className="text-xs text-slate-400">
                    {new Date(t.created_at).toLocaleString()}
                  </span>
                  <button
                    onClick={() => revokeToken(t.id)}
                    disabled={tokenBusy}
                    className="btn-outline ml-auto text-xs text-red-500"
                  >
                    吊销
                  </button>
                </div>
              ))}
            </div>
          )}
        </section>
      )}
    </div>
  );
}
