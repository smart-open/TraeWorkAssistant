import { Modal } from '../../components/ui';
import type { AccountView } from '../../types';

/** 删除账号确认弹框（禁 window.confirm，红线） */
export function DeleteAccountConfirmModal({
  target,
  onClose,
  onConfirm,
}: {
  target: AccountView | null;
  onClose: () => void;
  onConfirm: () => void;
}) {
  return (
    <Modal
      open={target != null}
      onClose={onClose}
      title="删除账号"
      footer={
        <>
          <button className="btn-outline" onClick={onClose}>取消</button>
          <button className="btn-primary !bg-rose-600 hover:!bg-rose-500" onClick={onConfirm}>确认删除</button>
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

/** 删除快照确认弹框（禁 window.confirm，红线） */
export function DeleteSlotConfirmModal({
  slot,
  onClose,
  onConfirm,
}: {
  slot: string | null;
  onClose: () => void;
  onConfirm: () => void;
}) {
  return (
    <Modal
      open={slot != null}
      onClose={onClose}
      title="删除快照"
      footer={
        <>
          <button className="btn-outline" onClick={onClose}>取消</button>
          <button className="btn-primary !bg-rose-600 hover:!bg-rose-500" onClick={onConfirm}>确认删除</button>
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
