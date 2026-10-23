import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { SyncStatus } from '../types/sync';

/**
 * 同步状态:挂载时拉一次 `get_sync_status`,之后靠后端 `sync-status` 事件更新。
 * 不做轮询 —— 后端每次同步周期结束都会 emit,轮询纯属浪费。
 * 返回 null 表示还没拿到初值(状态栏/设置面板据此不渲染状态点)。
 */
export function useSyncStatus(): SyncStatus | null {
  const [status, setStatus] = useState<SyncStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    invoke<SyncStatus>('get_sync_status')
      .then((s) => {
        if (!cancelled) setStatus(s);
      })
      .catch(() => {
        // 后端还没接线 / 命令不存在时静默:同步是可选功能,不能因此打扰用户
      });

    const unlisten = listen<SyncStatus>('sync-status', (event) => {
      if (!cancelled) setStatus(event.payload);
    });

    return () => {
      cancelled = true;
      unlisten.then((fn) => fn()).catch(() => {});
    };
  }, []);

  return status;
}
