import { useEffect, useMemo, useRef, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import { useClipboardStore } from '../stores/clipboardStore';
import { useSyncStatus } from '../hooks/useSyncStatus';
import { queryItems } from '../lib/db';
import { formatBytes } from '../lib/format';
import { ArrowUpIcon } from './icons';

// 排除原因 → 状态栏文案。用「未记录」而不是「已排除」:后者是内部术语,
// 普通用户看不懂"排除"意味着什么,「未记录」直接说明结果
const EXCLUSION_LABELS: Record<string, string> = {
  app: '未记录:来自黑名单软件',
  pattern: '未记录:命中匹配规则',
  sensitive: '未记录:疑似密钥或卡号',
};

export function StatusBar() {
  const items = useClipboardStore((s) => s.items);
  const isPaused = useClipboardStore((s) => s.isPaused);
  const [dbSize, setDbSize] = useState<number | null>(null);
  const [pasteMode, setPasteMode] = useState(false);
  const [appVersion, setAppVersion] = useState('');
  // 携带自增序号:连续排除同一原因时 state 值不变,不带序号的话
  // React 不会重渲染,定时器也不会重置
  const [excluded, setExcluded] = useState<{ reason: string; hash: string; seq: number } | null>(
    null
  );
  const excludeSeq = useRef(0);
  // 短信验证码捕获提示(同样带序号,理由同上)
  const [smsCode, setSmsCode] = useState<{ code: string; sender: string; seq: number } | null>(
    null
  );
  const smsSeq = useRef(0);
  const totalCount = items.length;
  const favoriteCount = useMemo(() => items.filter((i) => i.is_favorite).length, [items]);
  // 云同步状态:靠后端 sync-status 事件推送,不轮询;未启用时不显示
  const syncStatus = useSyncStatus();

  useEffect(() => {
    getVersion().then(setAppVersion).catch(() => {});
  }, []);

  // 排除规则命中时不入库,若不提示就成了静默丢数据——规则配得过宽时
  // 用户会以为应用坏了,这里给一个短暂可见的提示
  useEffect(() => {
    const unlisten = listen<{ reason: string; hash: string }>('item-excluded', (event) => {
      excludeSeq.current += 1;
      setExcluded({
        reason: event.payload.reason,
        hash: event.payload.hash,
        seq: excludeSeq.current,
      });
    });
    return () => { unlisten.then((fn) => fn()); };
  }, []);

  // 8 秒而非常见的 4 秒:这是用户唯一的补救机会。内容被排除时后端会把 hash
  // 写进 last_hash,所以错过这轮后再复制同一份内容不会有任何反应(去重直接
  // 跳过,提示条也不会再出现),得先复制点别的把 last_hash 冲掉才行 ——
  // 这个门道用户不可能自己想到。既然提示条现在带可点击的操作,就得多留点时间
  useEffect(() => {
    if (!excluded) return;
    const timer = setTimeout(() => setExcluded(null), 8000);
    return () => clearTimeout(timer);
  }, [excluded]);

  // 误判时的自救路径。没有它,被误排除的内容再复制多少次都进不了历史,
  // 用户只能整体关掉敏感识别 —— 那等于把整个功能废掉
  const handleKeepExcluded = async () => {
    if (!excluded) return;
    try {
      await invoke('allow_excluded_item', { hash: excluded.hash });
      setExcluded(null);
    } catch (err) {
      console.error('Failed to allow excluded item:', err);
    }
  };

  // 短信验证码:后端捕获成功后发事件,这里弹居中提示。
  // 后端已同时把验证码写进剪贴板(历史列表里也能翻到),提示只是告知
  useEffect(() => {
    const unlisten = listen<{ code: string; sender: string }>('sms-code-copied', (event) => {
      smsSeq.current += 1;
      setSmsCode({
        code: event.payload.code,
        sender: event.payload.sender,
        seq: smsSeq.current,
      });
    });
    return () => { unlisten.then((fn) => fn()); };
  }, []);

  // 验证码提示 8 秒自动消失(可点「知道了」提前关闭)。
  // 验证码有时效性,不需要像排除提示那样强调唯一补救机会,时长对齐即可
  useEffect(() => {
    if (!smsCode) return;
    const timer = setTimeout(() => setSmsCode(null), 8000);
    return () => clearTimeout(timer);
  }, [smsCode]);

  // 主动关闭(知道了按钮)
  const handleDismissExcluded = () => setExcluded(null);

  useEffect(() => {
    (async () => {
      try {
        const mode = await invoke<boolean>('get_paste_mode');
        setPasteMode(mode);
      } catch (err) {
        console.error('Failed to load paste mode:', err);
      }
    })();
    const unlisten = listen<boolean>('paste-mode-changed', (event) => {
      setPasteMode(event.payload);
    });
    return () => { unlisten.then((fn) => fn()); };
  }, []);

  // 复制模式仅在需要时手动切换(如资源管理器中避免误触发 Ctrl+V)
  const handleTogglePasteMode = async () => {
    const next = !pasteMode;
    try {
      await invoke('set_paste_mode', { inputMode: next });
      setPasteMode(next);
    } catch (err) {
      console.error('Failed to set paste mode:', err);
    }
  };

  useEffect(() => {
    let cancelled = false;
    const loadSize = async () => {
      try {
        const rows = await queryItems<{ size: number }>(
          "SELECT (SELECT page_count FROM pragma_page_count()) * (SELECT page_size FROM pragma_page_size()) AS size"
        );
        if (!cancelled && rows.length > 0) setDbSize(rows[0].size);
      } catch (err) {
        console.error('Failed to load db size:', err);
      }
    };
    loadSize();
    const timer = setInterval(loadSize, 10000);
    return () => { cancelled = true; clearInterval(timer); };
    // 轮询自身感知变化即可;依赖 totalCount 会在每次条数变化时重建定时器
  }, []);

  return (
    <div className="clip-statusbar flex items-center justify-between h-9 px-4 border-t border-hairline text-[11px] text-faint tabular-nums gap-3 @max-narrow:h-8 @max-narrow:px-2.5 @max-narrow:text-[10px] @min-wide:h-10 @min-wide:px-5 @min-wide:text-[12px]">
      {/* 左:统计。文本不换行(nowrap),容器 min-w-0+overflow-hidden
          允许窗口极窄时被裁剪。dbSize/appVersion 是次要信息,窄屏隐藏。
          右:粘贴模式+状态。flex-shrink-0 防止按钮被压成两行 */}
      <div className="flex items-center gap-2 min-w-0 flex-1 overflow-hidden">
        <span className="whitespace-nowrap">{totalCount} 条</span>
        <span aria-hidden="true" className="opacity-40 select-none">·</span>
        <span className="whitespace-nowrap">{favoriteCount} 收藏</span>
        {dbSize !== null && (
          <>
            <span aria-hidden="true" className="hidden @min-wide:inline opacity-40 select-none">·</span>
            <span className="hidden @min-wide:inline whitespace-nowrap">{formatBytes(dbSize)}</span>
          </>
        )}
        {appVersion && (
          // flex-shrink-0 保证版本号永远不被裁剪;不写 hidden md:inline
          // 让它在任何宽度下都可见(版本号是短文本,不会显著挤占空间,
          // 而且测试/调试时需要明确知道当前跑的是哪个版本)
          <>
            <span aria-hidden="true" className="opacity-40 select-none">·</span>
            <span className="whitespace-nowrap flex-shrink-0">v{appVersion}</span>
          </>
        )}
      </div>
      <div className="flex items-center gap-3 flex-shrink-0">
        <button
          onClick={handleTogglePasteMode}
          title="切换单击条目的行为(粘贴到之前点击的输入框 / 仅复制到剪贴板)"
          className={`flex items-center gap-1.5 px-[11px] py-[3px] rounded-full border transition-[background-color,color,border-color] duration-150 whitespace-nowrap @max-narrow:px-2 @max-narrow:py-[2px] ${
            pasteMode
              ? 'bg-accent-soft text-accent font-semibold border-accent-ring'
              : 'bg-transparent border-hairline text-muted hover:bg-app hover:text-ink'
          }`}
        >
          {pasteMode && <ArrowUpIcon size={11} />}
          {pasteMode ? '单击粘贴' : '单击复制'}
        </button>
        <div className="flex items-center gap-1.5 whitespace-nowrap">
          <span
            className={`w-1.5 h-1.5 rounded-full ${isPaused ? 'bg-warn' : 'bg-ok motion-safe:animate-pulse'}`}
            title={isPaused ? '已暂停' : '监听中'}
          />
          <span>{isPaused ? '已暂停' : '监听中'}</span>
        </div>
        {syncStatus && syncStatus.state !== 'disabled' && (
          // 云同步状态点:disabled 不渲染(没开同步的用户不该看到多余的点)
          <div className="flex items-center whitespace-nowrap">
            <span
              role="img"
              aria-label="云同步状态"
              className={`w-1.5 h-1.5 rounded-full ${
                syncStatus.state === 'syncing'
                  ? 'bg-accent motion-safe:animate-pulse'
                  : syncStatus.state === 'error'
                    ? 'bg-danger'
                    : 'bg-ok'
              }`}
              title={syncStatus.last_error || '已同步'}
            />
          </div>
        )}
      </div>

      {/* 两类提示共用同一个居中容器 —— 这点很关键:若各自 fixed inset-0
          居中,同时出现时两张卡片位置完全重合,后者会把前者盖掉(例如先
          命中排除规则、8 秒内又收到验证码)。放进同一个 flex-col 容器,
          它们就自动上下堆叠。
          外层 pointer-events-none 让背景仍可点;卡片本身 pointer-events-auto
          让按钮可点。8 秒自动消失,也可点"知道了"主动关闭。
          无障碍:每张卡片各自 role="status" + aria-live="polite" ——
          非模态短暂通知的标准模式。不要用 role="alertdialog"(那是模态
          对话框,会带焦点陷阱,跟这里的非模态行为不符) */}
      {(excluded || smsCode) && (
        <div className="fixed inset-0 z-[9999] pointer-events-none flex flex-col items-center justify-center gap-3">
          {excluded && (
            <div
              role="status"
              aria-live="polite"
              aria-label="排除规则提示"
              className="clip-dialog pointer-events-auto bg-surface rounded-[14px] shadow-dialog border border-hairline p-5 max-w-[360px] flex flex-col items-center gap-3 @max-narrow:p-4 @max-narrow:max-w-[92%]"
            >
              <div className="flex items-center gap-2">
                <span className="w-2 h-2 rounded-full bg-warn flex-shrink-0" aria-hidden="true" />
                <span className="text-[14px] font-semibold text-ink">
                  {EXCLUSION_LABELS[excluded.reason] ?? '未记录'}
                </span>
              </div>
              <p className="text-[12.5px] text-faint text-center leading-relaxed">
                这段内容符合排除规则,没有保存到历史里。如果是误判,可以点下面的按钮找回。
              </p>
              <div className="flex items-center gap-2 mt-1">
                <button
                  onClick={handleDismissExcluded}
                  className="h-[32px] px-4 text-[12.5px] rounded-[9px] text-muted hover:bg-hairline transition-colors duration-150"
                >
                  知道了
                </button>
                <button
                  onClick={handleKeepExcluded}
                  className="h-[32px] px-4 text-[12.5px] rounded-[9px] bg-accent-soft text-accent font-semibold ring-1 ring-inset ring-accent-ring hover:bg-accent hover:text-on-accent transition-colors duration-150"
                >
                  仍要记录
                </button>
              </div>
            </div>
          )}
          {/* 验证码已复制:与排除提示同款居中卡片,绿色圆点区分(这是
              正面通知而非警告) */}
          {smsCode && (
            <div
              role="status"
              aria-live="polite"
              aria-label="验证码已复制提示"
              className="clip-dialog pointer-events-auto bg-surface rounded-[14px] shadow-dialog border border-hairline p-5 max-w-[360px] flex flex-col items-center gap-3 @max-narrow:p-4 @max-narrow:max-w-[92%]"
            >
              <div className="flex items-center gap-2">
                <span className="w-2 h-2 rounded-full bg-ok flex-shrink-0" aria-hidden="true" />
                <span className="text-[14px] font-semibold text-ink">验证码已复制</span>
              </div>
              <p className="text-[12.5px] text-faint text-center leading-relaxed">
                来自 {smsCode.sender || '短信'} 的验证码{' '}
                <span className="text-ink font-mono font-semibold">{smsCode.code}</span>{' '}
                已写入剪贴板，直接粘贴即可
              </p>
              <div className="flex items-center gap-2 mt-1">
                <button
                  onClick={() => setSmsCode(null)}
                  className="h-[32px] px-4 text-[12.5px] rounded-[9px] text-muted hover:bg-hairline transition-colors duration-150"
                >
                  知道了
                </button>
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
