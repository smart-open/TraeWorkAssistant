import { Modal } from '../../components/ui';
import type { AccountView } from '../../types';

/** 删除账号确认弹框（禁 window.confirm，红线）。
 *  busy（F9）：确认请求 pending 期间禁用按钮并阻止关闭，防 1s withMinDelay 窗口内连点二次触发 */
export function DeleteAccountConfirmModal({
  target,
  busy = false,
  onClose,
  onConfirm,
}: {
  target: AccountView | null;
  busy?: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  return (
    <Modal
      open={target != null}
      onClose={() => {
        if (!busy) onClose();
      }}
      title="删除账号"
      footer={
        <>
          <button className="btn-outline" onClick={onClose} disabled={busy}>取消</button>
          <button className="btn-primary !bg-rose-600 hover:!bg-rose-500" onClick={onConfirm} disabled={busy}>
            {busy ? '删除中…' : '确认删除'}
          </button>
        </>
      }
    >
      <div className="text-sm">
        确认删除账号「{target?.name}」？
        {target?.device_id_masked && (
          <div className="mt-1 text-xs text-slate-400">将一并清理该账号的设备 ID。</div>
        )}
      </div>
    </Modal>
  );
}

/** 删除快照确认弹框（禁 window.confirm，红线）。busy 语义同上（F9） */
export function DeleteSlotConfirmModal({
  slot,
  busy = false,
  onClose,
  onConfirm,
}: {
  slot: string | null;
  busy?: boolean;
  onClose: () => void;
  onConfirm: () => void;
}) {
  return (
    <Modal
      open={slot != null}
      onClose={() => {
        if (!busy) onClose();
      }}
      title="删除快照"
      footer={
        <>
          <button className="btn-outline" onClick={onClose} disabled={busy}>取消</button>
          <button className="btn-primary !bg-rose-600 hover:!bg-rose-500" onClick={onConfirm} disabled={busy}>
            {busy ? '删除中…' : '确认删除'}
          </button>
        </>
      }
    >
      <div className="text-sm">
        确认删除快照「{slot}」？
        <div className="mt-1 text-xs text-slate-400">删除后需重新保存登录态才能再次切换到该账号。</div>
      </div>
    </Modal>
  );
}
