import { useState } from 'react';
import { FolderOpen, Info, RefreshCw, X } from 'lucide-react';
import { useAppStore } from '../store';
import { api } from '../lib/tauri';

/**
 * 检测 certs/ca.cer 是否已生成。仅生成成功后才值得弹手动导入指引
 * （ca.cer 不存在 = 证书尚未生成或生成失败，指引里「找 ca.cer」无意义）。
 */
export async function caCerExists(): Promise<boolean> {
  try {
    return (await api.cert.status()).ca_exists;
  } catch {
    return false;
  }
}

/**
 * 证书安装失败后的手动导入三步指引（issue #12/#79：手动找 %APPDATA%\AIWorkAssistant\certs\ca.cer 成本高）。
 * 以 amber 提示条形式嵌入引导卡片（无外层 card，带 border-t），替代纯报错 toast；
 * 独立卡片场景（Dashboard 横幅）用 `<div className="card overflow-hidden">` 包裹即可。
 */
export default function CertManualGuide({ onClose }: { onClose?: () => void }) {
  const toast = useAppStore((s) => s.pushToast);
  const [checking, setChecking] = useState(false);

  const openFolder = async () => {
    try {
      await api.cert.openFolder();
    } catch (e) {
      toast('error', String(e));
    }
  };

  const recheck = async () => {
    setChecking(true);
    try {
      // 直接查后端状态并区分「检测失败 / 未安装 / ca.cer 缺失」，不复用吞错的 refreshCert
      const st = await api.cert.status();
      useAppStore.setState({ certInstalled: st.installed });
      if (st.installed) {
        toast('success', '检测到证书已安装，浏览器重启后即可信任代理证书');
      } else if (st.ca_exists) {
        toast('warn', '仍未检测到证书：请确认第 2 步存储选了「受信任的根证书颁发机构」，安全警告点了「是」');
      } else {
        toast('warn', '证书文件 ca.cer 不存在（可能尚未生成或生成失败），请先重新执行「安装证书」');
      }
    } catch (e) {
      toast('error', `证书状态检测失败：${String(e)}`);
    } finally {
      setChecking(false);
    }
  };

  return (
    <div className="border-t border-slate-100 bg-amber-50/60 px-4 py-3 text-sm text-amber-700 dark:border-zinc-800 dark:bg-amber-500/10 dark:text-amber-300">
      <div className="mb-1.5 flex items-center justify-between">
        <div className="flex items-center gap-1.5 font-medium">
          <Info size={14} /> 自动安装失败？手动导入 3 步搞定
        </div>
        {onClose && (
          <button
            onClick={onClose}
            title="收起指引"
            className="rounded p-0.5 transition hover:bg-amber-100 dark:hover:bg-amber-900/40"
          >
            <X size={14} />
          </button>
        )}
      </div>
      <ol className="list-decimal space-y-1 pl-5 text-xs">
        <li>
          点下方「打开证书文件夹」，找到并双击{' '}
          <code className="rounded bg-amber-100 px-1 py-0.5 font-mono text-amber-800 dark:bg-amber-900/40 dark:text-amber-200">
            ca.cer
          </code>{' '}
          → 点「安装证书…」。
        </li>
        <li>
          存储位置保持「当前用户」→ 下一步选「根据证书类型自动选择证书存储」（即「受信任的根证书颁发机构」）→
          完成；弹出安全警告时点「是」。
        </li>
        <li>回到这里点「重新检测」确认（浏览器需重启后才会信任新证书）。</li>
      </ol>
      <div className="mt-2 flex items-center gap-2">
        <button onClick={() => void openFolder()} className="btn-outline px-2.5 py-1.5 text-xs">
          <FolderOpen size={14} /> 打开证书文件夹
        </button>
        <button onClick={() => void recheck()} disabled={checking} className="btn-outline px-2.5 py-1.5 text-xs">
          <RefreshCw size={14} className={checking ? 'animate-spin' : ''} /> {checking ? '检测中…' : '重新检测'}
        </button>
      </div>
    </div>
  );
}
