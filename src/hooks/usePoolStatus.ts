import { useEffect, useMemo, useState } from 'react';
import { api } from '../lib/tauri';
import type { PoolStatus } from '../types';

/** 池种类 → 对应 pool_status 命令（issue #178 账号列表限流徽标数据源） */
export type PoolKind = 'trae' | 'wb' | 'qoder' | 'buddyai';

const FETCHERS: Record<PoolKind, () => Promise<PoolStatus[]>> = {
  trae: () => api.apiServer.poolStatus(),
  wb: () => api.apiServer.wbPoolStatus(),
  qoder: () => api.apiServer.qoderPoolStatus(),
  buddyai: () => api.apiServer.buddyAiPoolStatus(),
};

/**
 * 池实时状态轮询（3s，对齐 QoderApiService/ApiService 页内轮询惯例）。
 *
 * 返回 uid → PoolStatus 联表映射，供账号列表/到期列表按行注入**运行期**
 * 限流/冷却状态（issue #178）：429 软冷却与积分耗尽硬冷却只存在于 API
 * 网关内存池（不落 account_cooldowns 持久表），账号 View 快照拿不到，
 * 必须经 pool_status 系命令实时读取。
 *
 * API 网关未启动 / 查询失败时降级为空 Map（冷却本来也随网关重启清零，
 * 不显示即正确语义）；倒计时跳动由 Badge 组件内部 interval 负责，本 hook
 * 固定 3s 刷新数据即可。
 */
export function usePoolStatus(kind: PoolKind): Map<string, PoolStatus> {
  const [list, setList] = useState<PoolStatus[]>([]);

  useEffect(() => {
    // 无条件轮询，不以前端 running 门控：托盘启停网关在 Rust 侧直接生效、不经
    // 前端 store，running 快照滞后误判「未启动」会清空列表、丢运行期冷却徽标；
    // 查询失败（网关真未启动）走 catch 清空即为正确语义（冷却随网关重启清零）
    let alive = true;
    const fetch = async () => {
      try {
        const st = await FETCHERS[kind]();
        if (alive) setList(st);
      } catch {
        // 查询失败（网关刚停/竞态）：清空展示，下次轮询自动恢复
        if (alive) setList([]);
      }
    };
    void fetch();
    const id = setInterval(() => void fetch(), 3000);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [kind]);

  return useMemo(() => new Map(list.map((s) => [s.uid, s])), [list]);
}
