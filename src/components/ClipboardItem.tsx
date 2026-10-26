import { memo, useState, useEffect, useCallback, useRef } from 'react';
import { invoke, convertFileSrc } from '@tauri-apps/api/core';
import { useClipboardStore } from '../stores/clipboardStore';
import { useContinuousPasteStore } from '../stores/continuousPasteStore';
import { dbTimeToDate, formatBytes } from '../lib/format';
import { buildRichPreviewSrcDoc } from '../lib/richPreview';
import type { ClipboardItem as ClipboardItemType, ClipboardType } from '../types/clipboard';
import { ClipboardType as CT } from '../types/clipboard';
import {
  TextIcon,
  LinkIcon,
  ImageIcon,
  FileIcon,
  StarIcon,
  StarOutlineIcon,
  FolderIcon,
  FolderFillIcon,
  TrashIcon,
  EyeIcon,
  CheckIcon,
} from './icons';

// 图片缩略图经 asset 协议按需加载(不经 IPC/base64);
// 文件大小按需取一次后缓存为数字,几乎不占内存
const fileSizeCache = new Map<string, number>();

async function fetchAssetSize(url: string): Promise<number | null> {
  try {
    const resp = await fetch(url);
    if (!resp.ok) return null;
    return (await resp.blob()).size;
  } catch {
    return null;
  }
}

// 悬停气泡的富文本片段缓存:同一条目反复悬停不重复走 IPC + 文件读取。
// 取舍:items.id 是 INTEGER PRIMARY KEY AUTOINCREMENT(永不复用),按 id
// 缓存不会串条目;条目被编辑保存后 html_path 置 NULL,气泡按 html_path
// 门禁直接退回纯文本,不会读到过期片段。只缓存成功结果,失败下次还能重试。
const richPreviewCache = new Map<number, string | null>();

async function fetchRichPreview(id: number): Promise<string | null> {
  if (richPreviewCache.has(id)) return richPreviewCache.get(id) ?? null;
  try {
    const res = await invoke<{ html: string | null }>('get_rich_preview', { id });
    const html = res?.html ?? null;
    richPreviewCache.set(id, html);
    return html;
  } catch {
    return null;
  }
}

interface Props {
  item: ClipboardItemType;
  /** 1 起的列表序号:仅 A/C 风格用 CSS 显示(.clip-idx 默认 display:none) */
  index: number;
  onActivate: (item: ClipboardItemType) => void;
  onDelete: (id: number) => void;
  onToggleFavorite: (id: number) => void;
  onPreview: (item: ClipboardItemType) => void;
}

const typeIcons: Record<ClipboardType, typeof TextIcon> = {
  [CT.Text]: TextIcon,
  [CT.Link]: LinkIcon,
  [CT.Image]: ImageIcon,
  [CT.File]: FileIcon,
};

// 文件类内容是多行路径:主行显示首个文件名,次行显示完整路径(多个时注明数量)
function getFileMeta(content: string): { name: string; meta: string } {
  const lines = content.split('\n').filter(Boolean);
  const first = lines[0] ?? '';
  const name = first.split('\\').pop() || first;
  const meta = lines.length > 1 ? `${lines.length} 个文件 · ${first}` : first;
  return { name, meta };
}

function formatImageTitle(dateStr: string): string {
  const d = dbTimeToDate(dateStr);
  if (!d) return '图片';
  const pad = (n: number) => String(n).padStart(2, '0');
  return `图片 ${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function formatTime(dateStr: string): string {
  const date = dbTimeToDate(dateStr);
  if (!date) return '';
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  const diffMins = Math.floor(diffMs / 60000);
  const diffHours = Math.floor(diffMins / 60);
  const diffDays = Math.floor(diffHours / 24);

  if (diffMins < 1) return '刚刚';
  if (diffMins < 60) return `${diffMins}分钟前`;
  if (diffHours < 24) return `${diffHours}小时前`;
  return `${diffDays}天前`;
}

export const ClipboardItemCard = memo(function ClipboardItemCard({
  item,
  index,
  onActivate,
  onDelete,
  onToggleFavorite,
  onPreview,
}: Props) {
  const isImage = item.type === CT.Image;
  const imageSrc = isImage && item.file_path ? convertFileSrc(item.file_path) : null;
  const [imageFailed, setImageFailed] = useState(false);
  const [imgSize, setImgSize] = useState<{ w: number; h: number } | null>(null);
  const [fileSize, setFileSize] = useState<number | null>(() =>
    item.file_path ? fileSizeCache.get(item.file_path) ?? null : null
  );
  const [mousePos, setMousePos] = useState<{ x: number; y: number } | null>(null);
  // 跟随窗口尺寸:不挂到 state 上,用户缩窗时预览会停留在旧的 innerWidth
  // 计算的位置,被窗口边缘切掉。resize 事件让它进入重渲染即可
  const [viewport, setViewport] = useState(() => ({
    w: window.innerWidth,
    h: window.innerHeight,
  }));
  useEffect(() => {
    const onResize = () => setViewport({ w: window.innerWidth, h: window.innerHeight });
    window.addEventListener('resize', onResize);
    return () => window.removeEventListener('resize', onResize);
  }, []);
  const [groupMenuOpen, setGroupMenuOpen] = useState(false);
  // 悬停「眼睛」时的全文气泡:position:fixed 跟随按钮矩形定位,
  // 不参与虚拟列表的行高计算,避免悬停时列表抖动。
  // 气泡可交互(pointer-events 默认值):滚轮直接滚动浏览长文本,
  // 移入气泡不会关闭 —— 靠「按钮 leave 延时关闭 + 气泡 enter 取消」接力
  const [contentTip, setContentTip] = useState<
    { left: number; top: number; flipUp: boolean; maxHeight: number } | null
  >(null);
  const tipCloseTimer = useRef<number | undefined>(undefined);
  // 气泡里的富文本片段:连 id 一起存,换条目时旧片段立即失效。
  // 未到位时气泡仍是纯文本(渐进增强,不阻塞 hover)
  const [tipRich, setTipRich] = useState<{ id: number; html: string | null } | null>(null);
  // 请求序号:气泡关闭或换条目时推进,晚到的响应据此丢弃
  const tipRichReq = useRef(0);
  const groups = useClipboardStore((s) => s.groups);
  // 连续粘贴(框选模式):选中态用明显的包围框描边 + 粘贴顺序徽标,
  // 已粘贴条目整体变淡并打勾。store 为空映射/空集合时全部为默认值,
  // 普通模式下零开销
  const boxMode = useContinuousPasteStore((s) => s.mode);
  const boxSelected = useContinuousPasteStore(
    (s) => s.mode && (s.selectedIds.has(item.id) || s.bandPreview.has(item.id))
  );
  const boxOrder = useContinuousPasteStore((s) => s.orderById.get(item.id) ?? null);
  // 已粘贴标记只在框选模式内生效:模式结束后 pastedIds 残留也不该继续高亮
  const boxUsed = useContinuousPasteStore((s) => s.mode && s.pastedIds.has(item.id));

  useEffect(() => () => window.clearTimeout(tipCloseTimer.current), []);

  // 组件是 memo 的,虚拟列表会用同一个实例渲染另一行:item 变化时
  // 丢掉上一条的片段并作废在途请求。html_path 也要进依赖 —— 条目被
  // 编辑保存后 id 不变但富格式已清空,缓存里的旧片段必须一起失效
  useEffect(() => {
    setTipRich(null);
    tipRichReq.current += 1;
  }, [item.id, item.html_path]);

  const handleEyeEnter = useCallback((e: React.MouseEvent<HTMLButtonElement>) => {
    window.clearTimeout(tipCloseTimer.current);
    const rect = e.currentTarget.getBoundingClientRect();
    const TIP_W = 320; // 与气泡 maxWidth 保持一致,用于水平夹取
    const GAP = 8;
    const left = Math.min(
      Math.max(GAP, rect.right - TIP_W),
      Math.max(GAP, window.innerWidth - TIP_W - GAP),
    );
    // 按按钮上/下方的**实际剩余空间**决定弹出方向与最大高度:
    // 之前按"下半屏就向上翻"的粗略启发式,气泡仍可能越过窗口底边被截断
    const spaceBelow = window.innerHeight - rect.bottom - GAP;
    const spaceAbove = rect.top - GAP;
    const flipUp = spaceAbove > spaceBelow;
    const maxHeight = Math.max(
      120,
      Math.min(window.innerHeight * 0.45, flipUp ? spaceAbove : spaceBelow),
    );
    setContentTip({ left, top: flipUp ? rect.top - GAP : rect.bottom + GAP, flipUp, maxHeight });

    // 富格式条目:顺手拉净化片段,到位后气泡换成图文 iframe;
    // 无 html_path / 拉取失败 / 拉取中,气泡都维持现有纯文本形态
    if (!item.html_path) return;
    const seq = ++tipRichReq.current;
    void fetchRichPreview(item.id).then((html) => {
      // 气泡已关闭、已换条目或又发起了新请求 → 丢弃这次响应
      if (tipRichReq.current !== seq) return;
      setTipRich({ id: item.id, html });
    });
  }, [item.id, item.html_path]);

  // 离开按钮后延迟一小段再关,给鼠标移进气泡留出时间差
  const handleEyeLeave = useCallback(() => {
    window.clearTimeout(tipCloseTimer.current);
    tipCloseTimer.current = window.setTimeout(() => {
      setContentTip(null);
      // 气泡关闭后到达的响应一律作废
      tipRichReq.current += 1;
    }, 120);
  }, []);
  const cancelTipClose = useCallback(() => window.clearTimeout(tipCloseTimer.current), []);

  // 只渲染当前条目的片段(虚拟列表复用实例时旧片段立刻失效);
  // 再压一道 html_path 门禁:富格式被清空后即使状态还没清也不显示旧图文
  const tipRichHtml =
    tipRich && tipRich.id === item.id && item.html_path ? tipRich.html : null;

  const handleMouseEnter = useCallback((e: React.MouseEvent) => {
    setMousePos({ x: e.clientX, y: e.clientY });
  }, []);

  const handleMouseMove = useCallback((e: React.MouseEvent) => {
    setMousePos({ x: e.clientX, y: e.clientY });
  }, []);

  const handleMouseLeave = useCallback(() => {
    setMousePos(null);
  }, []);

  const handleSetGroup = useCallback(async (itemId: number, groupId: number | null) => {
    setGroupMenuOpen(false);
    try {
      await invoke('set_item_group', { itemId, groupId });
      // 列表刷新由 clipboard-changed 事件触发
    } catch (err) {
      console.error('Set group failed:', err);
    }
  }, []);

  // Fix #5: Reset imageFailed when item.file_path changes
  useEffect(() => {
    setImageFailed(false);
    setImgSize(null);
  }, [item.file_path]);

  // 文件大小按需取一次(数字级缓存,几乎不占内存)
  useEffect(() => {
    if (!imageSrc || !item.file_path) return;
    const filePath = item.file_path; // capture locally to avoid ! assertion
    if (fileSize !== null || fileSizeCache.has(filePath)) {
      if (fileSize === null) setFileSize(fileSizeCache.get(filePath) ?? null);
      return;
    }
    let cancelled = false;
    fetchAssetSize(imageSrc).then((size) => {
      if (cancelled) return;
      if (size !== null) fileSizeCache.set(filePath, size);
      setFileSize(size);
    });
    return () => { cancelled = true; };
  }, [imageSrc, item.file_path, fileSize]);

  const renderImagePreview = () => {
    // 设计尺寸:image 350x300 + 容器 18px(2*8 padding + 2*1 border)= 368x318。
    // 窗口比这个尺寸还小时,按比例缩小预览,保证不超出可视范围
    const DESIGN_W = 368;
    const DESIGN_H = 318;
    const MARGIN = 10; // 视口内边距,留出空隙
    const GAP = 16;

    // 可用空间 = 视口减边距;设下界避免极端情况下算出 0 或负数
    const availW = Math.max(120, viewport.w - MARGIN * 2);
    const availH = Math.max(120, viewport.h - MARGIN * 2);
    // 取最小缩放比,且绝不放大(夹到 1)
    const scale = Math.min(1, availW / DESIGN_W, availH / DESIGN_H);
    const imgW = Math.round(350 * scale);
    const imgH = Math.round(300 * scale);
    const totalW = imgW + 18; // 容器加上 padding+border
    const totalH = imgH + 18;

    // 边界 clamp 用真实(缩放后)尺寸,而不是设计尺寸
    let previewX = 0;
    let previewY = 0;
    if (mousePos) {
      previewX = mousePos.x + GAP;
      if (previewX + totalW > viewport.w) {
        previewX = mousePos.x - GAP - totalW;
        if (previewX < 0) previewX = 0;
      }
      previewY = Math.max(mousePos.y - 150, 0);
      if (previewY + totalH > viewport.h) {
        previewY = Math.max(0, viewport.h - totalH);
      }
    }

    if (imageSrc && !imageFailed) {
      const metaParts = [
        'PNG',
        imgSize ? `${imgSize.w} × ${imgSize.h}` : null,
        fileSize !== null ? formatBytes(fileSize) : null,
      ].filter(Boolean);
      return (
        <div className="relative flex items-center gap-2.5 min-w-0 flex-1">
          <img
            src={imageSrc}
            alt="[图片]"
            className="h-11 w-11 rounded-lg border border-hairline object-cover flex-shrink-0"
            onError={() => setImageFailed(true)}
            onLoad={(e) => {
              if (!imgSize) {
                setImgSize({ w: e.currentTarget.naturalWidth, h: e.currentTarget.naturalHeight });
              }
            }}
            onMouseEnter={handleMouseEnter}
            onMouseMove={handleMouseMove}
            onMouseLeave={handleMouseLeave}
          />
          <div className="min-w-0 flex-1">
            <p className="text-[13px] leading-snug text-ink truncate @max-narrow:text-[12.5px] @min-wide:text-[13.5px]">{formatImageTitle(item.created_at)}</p>
            <p className="mt-0.5 text-[11px] text-faint tabular-nums truncate @max-narrow:hidden">{metaParts.join(' · ')}</p>
          </div>
          {mousePos && (
            <div
              className="fixed z-[9999] pointer-events-none"
              style={{
                left: previewX,
                top: previewY,
              }}
            >
              <div className="clip-dialog bg-surface rounded-lg shadow-dialog border border-hairline p-2">
                <img
                  src={imageSrc}
                  alt="[图片预览]"
                  className="object-contain rounded"
                  style={{ maxWidth: imgW, maxHeight: imgH }}
                />
              </div>
            </div>
          )}
        </div>
      );
    }
    // Fallback: icon + text
    return (
      <p className="text-[13px] text-ink truncate">[图片]</p>
    );
  };

  return (
    <div
      data-selected={boxSelected || undefined}
      data-pasted={boxUsed || undefined}
      className={`group clip-row relative flex items-center gap-2.5 min-h-[52px] p-[10px] rounded-[10px] cursor-pointer border border-transparent transition-[background-color,border-color] duration-150 hover:bg-surface hover:border-hairline @max-narrow:gap-2 @max-narrow:min-h-[46px] @max-narrow:p-2 @min-wide:gap-3 @min-wide:min-h-[60px] @min-wide:p-3 ${
        boxSelected ? 'ring-2 ring-accent bg-accent-soft/50' : ''
      } ${boxMode && boxUsed ? 'opacity-50' : ''}`}
      onClick={(e) => {
        if (boxMode) {
          // 框选模式:单击只切换选中(Shift+点击区间补选),绝不触发系统粘贴;
          // 拖拽框选结束的那次 click 已被 ClipboardList 的捕获阶段拦截
          if (e.shiftKey) {
            useContinuousPasteStore.getState().selectRangeTo(item.id);
          } else {
            useContinuousPasteStore.getState().toggleSelect(item.id);
          }
          return;
        }
        onActivate(item);
      }}
    >
      {/* 序号栏:浮笺下 display:none(globals.css 的默认规则),A/C 段才显示 */}
      <span className="clip-idx" aria-hidden="true">{String(index).padStart(2, '0')}</span>
      {!(isImage && imageSrc && !imageFailed) && (
        <div className="clip-ico flex-shrink-0 flex items-center justify-center w-[26px] text-faint">
          {(() => {
            const TypeIcon = typeIcons[item.type];
            return <TypeIcon size={14} />;
          })()}
        </div>
      )}
      <div className="flex-1 min-w-0">
        {isImage ? (
          renderImagePreview()
        ) : item.type === CT.File ? (
          (() => {
            const { name, meta } = getFileMeta(item.content);
            return (
              <>
                <p className="text-[13px] leading-snug text-ink truncate @max-narrow:text-[12.5px] @min-wide:text-[13.5px]">{name}</p>
                <p className="mt-0.5 text-[11px] text-faint tabular-nums truncate @max-narrow:hidden">{meta}</p>
              </>
            );
          })()
        ) : (
          <p className={`text-[13px] leading-snug truncate @max-narrow:text-[12.5px] @min-wide:text-[13.5px] ${item.type === CT.Link ? 'text-accent' : 'text-ink'}`}>
            {item.preview || item.content}
          </p>
        )}
      </div>
      <div className="flex-shrink-0 flex items-center">
        {item.is_favorite && (
          <span className="text-star mr-2"><StarIcon size={13} /></span>
        )}
        {item.group_id !== null && (
          <span className="text-accent mr-2"><FolderFillIcon size={13} /></span>
        )}
        {/* 时间任何宽度下都显示完整格式:flex-shrink-0 保证自身不被压缩,
            空间不足时由正文行(flex-1 + truncate)让位 */}
        <span className="text-[11px] text-faint tabular-nums flex-shrink-0 whitespace-nowrap @min-wide:text-[12px]">
          {formatTime(item.last_used_at)}
        </span>
        {!boxMode && (
          <div className="clip-actions-wrap flex items-center pl-2 w-0 overflow-hidden group-hover:w-[118px] group-focus-within:w-[118px] transition-[width] duration-150 ease-out @max-narrow:pl-1 @max-narrow:group-hover:w-[98px] @max-narrow:group-focus-within:w-[98px] @min-wide:group-hover:w-[130px] @min-wide:group-focus-within:w-[130px]">
          <div className="clip-actions flex items-center gap-0.5 opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 transition-opacity duration-150">
            {!isImage && (
              <button
                onMouseDown={(e) => e.preventDefault()}
                onMouseEnter={handleEyeEnter}
                onMouseLeave={handleEyeLeave}
                onClick={(e) => { e.stopPropagation(); e.currentTarget.blur(); onPreview(item); }}
                className="flex items-center justify-center w-[26px] h-[26px] rounded-[7px] text-faint hover:bg-hairline hover:text-muted transition-colors @max-narrow:w-[22px] @max-narrow:h-[22px]"
                title="预览 / 编辑内容"
              >
                <EyeIcon size={14} />
              </button>
            )}
            <button
              onMouseDown={(e) => e.preventDefault()}
              onClick={(e) => { e.stopPropagation(); onToggleFavorite(item.id); }}
              className="flex items-center justify-center w-[26px] h-[26px] rounded-[7px] text-faint hover:bg-hairline hover:text-muted transition-colors @max-narrow:w-[22px] @max-narrow:h-[22px]"
              title={item.is_favorite ? '取消收藏' : '收藏'}
            >
              {item.is_favorite ? <StarIcon size={14} /> : <StarOutlineIcon size={14} />}
            </button>
            <button
              onMouseDown={(e) => e.preventDefault()}
              onClick={(e) => { e.stopPropagation(); setGroupMenuOpen((v) => !v); }}
              className="flex items-center justify-center w-[26px] h-[26px] rounded-[7px] text-faint hover:bg-hairline hover:text-muted transition-colors @max-narrow:w-[22px] @max-narrow:h-[22px]"
              title={item.group_id !== null ? '更改分组' : '归入分组'}
            >
              {item.group_id !== null ? <FolderFillIcon size={14} /> : <FolderIcon size={14} />}
            </button>
            <button
              onMouseDown={(e) => e.preventDefault()}
              onClick={(e) => { e.stopPropagation(); onDelete(item.id); }}
              className="flex items-center justify-center w-[26px] h-[26px] rounded-[7px] text-faint hover:bg-hairline hover:text-danger transition-colors @max-narrow:w-[22px] @max-narrow:h-[22px]"
              title="删除"
            >
              <TrashIcon size={14} />
            </button>
          </div>
        </div>
        )}
      </div>
      {contentTip && (
        <div
          // 外层只负责定位与 flipUp 的 translateY,动画用仅含 opacity 的 clip-fade-in:
          // clip-pop-in 会在动画结束时把 transform 覆盖为 none,导致向上弹出的气泡跳位。
          // 气泡可交互:滚轮滚动浏览全文;enter 取消按钮 leave 安排的延时关闭,
          // leave 重新安排;stopPropagation 防止点击气泡触发卡片「点击即粘贴」
          className="fixed z-[9999] clip-fade-in"
          style={{
            left: contentTip.left,
            top: contentTip.top,
            transform: contentTip.flipUp ? 'translateY(-100%)' : undefined,
          }}
          onMouseEnter={cancelTipClose}
          onMouseLeave={handleEyeLeave}
          onClick={(e) => e.stopPropagation()}
        >
          {tipRichHtml ? (
            // 富文本气泡:高度固定一档、滚动交给 iframe 自身;外层不再
            // overflow-y-auto,否则会出现内外双滚动条。
            // srcDoc 与弹窗共用 lib/richPreview(同款 CSP + 排版归一 + 主题色);
            // bg-surface 让 iframe 底色在载入前就与 srcDoc 的 palette 一致
            <iframe
              title="富文本预览"
              sandbox=""
              srcDoc={buildRichPreviewSrcDoc(tipRichHtml)}
              className="clip-dialog block w-[320px] h-[200px] rounded-lg shadow-dialog border border-hairline bg-surface"
            />
          ) : (
            <div
              className="clip-dialog bg-surface rounded-lg shadow-dialog border border-hairline px-3 py-2 max-w-[320px] overflow-y-auto"
              style={{ maxHeight: contentTip.maxHeight }}
            >
              <p className="text-[12.5px] leading-relaxed text-ink whitespace-pre-wrap break-words select-text">
                {item.content}
              </p>
            </div>
          )}
        </div>
      )}
      {groupMenuOpen && (
        <>
          <div className="fixed inset-0 z-40" onClick={(e) => { e.stopPropagation(); setGroupMenuOpen(false); }} />
          <div
            className="clip-dialog absolute right-[10px] top-full mt-1 z-50 min-w-[140px] bg-surface rounded-lg shadow-dialog border border-hairline py-1"
            onClick={(e) => e.stopPropagation()}
          >
            <button
              onClick={() => handleSetGroup(item.id, null)}
              className={`w-full text-left px-3 py-1.5 text-[13px] hover:bg-app ${
                item.group_id === null ? 'text-accent font-medium' : 'text-ink'
              }`}
            >
              {item.group_id === null ? '未分组' : '取消分组'}
            </button>
            {groups.map((g) => (
              <button
                key={g.id}
                onClick={() => handleSetGroup(item.id, g.id)}
                className={`w-full text-left px-3 py-1.5 text-[13px] hover:bg-app truncate ${
                  item.group_id === g.id ? 'text-accent font-medium' : 'text-ink'
                }`}
              >
                {g.icon} {g.name}
              </button>
            ))}
            {groups.length === 0 && (
              <p className="px-3 py-1.5 text-[11px] text-faint">暂无分组,可在分组栏新建</p>
            )}
          </div>
        </>
      )}
      {boxMode && boxSelected && !boxUsed && boxOrder !== null && (
        // 粘贴顺序徽标:按点选先后 —— 先点的第 1 条,后点的依次排后
        <span className="absolute -top-1.5 -right-1.5 z-20 flex items-center justify-center h-[18px] min-w-[18px] px-1 rounded-full bg-accent text-on-accent text-[10px] font-bold leading-none tabular-nums shadow-lift pointer-events-none">
          {boxOrder}
        </span>
      )}
      {boxMode && boxUsed && (
        <span className="absolute right-3 top-1/2 -translate-y-1/2 z-20 flex items-center gap-1 px-1.5 py-0.5 rounded-full bg-ok text-on-accent text-[10px] font-semibold pointer-events-none">
          <CheckIcon size={10} />
          已粘贴
        </span>
      )}
    </div>
  );
});
