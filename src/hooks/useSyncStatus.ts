import { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { SyncStatus } from '../types/sync';

/**
 * 同步状态:先注册 `sync-status` 监听,再拉一次 `get_sync_status`。
 *
 * 顺序不能反:若先 get 再 listen,后端恰好在两者之间从 syncing 切到 idle,
 * 这次 idle 事件就丢了;`startup` 模式下一轮同步要等重启/手动,UI 会永远
 * 停在「同步中」。注册监听后再拉一次,保证拿到的是一个不早于监听的快照。
 *
 * 另外在 `syncing` 期间做 2s 轻量轮询兜底(本地命令,不发网络请求):
 * 万一大对象上传卡住或事件丢失,状态至少能自愈成真实值。
 */
export function useSyncStatus(): SyncStatus | null {
  const [status, setStatus] = useState<SyncStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    const setup = async () => {
      try {
        const off = await listen<SyncStatus>('sync-status', (event) => {
          if (!cancelled) setStatus(event.payload);
        });
        if (cancelled) {
          off();
          return;
        }
        unlisten = off;
      } catch {
        // 事件系统不可用(测试/旧后端)时,下面的 get 仍能给出初值
      }

      try {
        const s = await invoke<SyncStatus>('get_sync_status');
        if (!cancelled) setStatus(s);
      } catch {
        // 后端还没接线 / 命令不存在时静默:同步是可选功能,不能因此打扰用户
      }
    };

    void setup();

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  // syncing 期间每 2s 拉一次真实状态,防止事件丢失后永久停在「同步中」
  useEffect(() => {
    if (status?.state !== 'syncing') return;
    let cancelled = false;
    const timer = setInterval(() => {
      invoke<SyncStatus>('get_sync_status')
        .then((s) => {
          if (!cancelled) setStatus(s);
        })
        .catch(() => {});
    }, 2000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [status?.state]);

  return status;
}
