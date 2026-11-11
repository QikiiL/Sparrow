import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { renderHook, act } from '@testing-library/react';
import type { RefObject } from 'react';
import { useRubberBandSelect } from '../useRubberBandSelect';

/** 容器视口矩形:高 400,上下各 28px 为自动滚动感应带 */
const VIEW = { top: 0, bottom: 400, left: 0, right: 320, width: 320, height: 400, x: 0, y: 0 };

function makeRect(top: number, height: number): DOMRect {
  return {
    top,
    bottom: top + height,
    left: 0,
    right: 320,
    width: 320,
    height,
    x: 0,
    y: top,
    toJSON: () => ({}),
  } as DOMRect;
}

interface Harness {
  container: HTMLDivElement;
  rows: HTMLDivElement[];
  ref: RefObject<HTMLDivElement | null>;
  onApply: ReturnType<typeof vi.fn>;
  onPreview: ReturnType<typeof vi.fn>;
  onBackgroundClick: ReturnType<typeof vi.fn>;
  mousedown: (clientY: number, clientX?: number) => void;
  mousemove: (clientY: number, clientX?: number) => void;
  mouseup: () => void;
  unmount: () => void;
  /** 推进一帧(执行当帧所有 rAF 回调,含新注册的下一帧) */
  frame: () => void;
  frames: (n: number) => void;
}

// 手动控制的 rAF:逐帧执行,便于断言"某帧滚了多少/是否重算命中"
let rafId = 0;
let rafCallbacks = new Map<number, FrameRequestCallback>();

function setup(
  opts: { scrollHeight?: number; clientHeight?: number; scrollTop?: number; rowCount?: number } = {}
): Harness {
  const { scrollHeight = 2000, clientHeight = 400, scrollTop = 0, rowCount = 8 } = opts;

  const container = document.createElement('div');
  // jsdom 无布局:scrollHeight/clientHeight 恒为 0,必须手动覆盖
  Object.defineProperty(container, 'scrollHeight', { value: scrollHeight, configurable: true });
  Object.defineProperty(container, 'clientHeight', { value: clientHeight, configurable: true });
  Object.defineProperty(container, 'scrollTop', { value: scrollTop, writable: true, configurable: true });
  container.getBoundingClientRect = () => VIEW as DOMRect;

  const rows: HTMLDivElement[] = [];
  for (let i = 0; i < rowCount; i++) {
    const row = document.createElement('div');
    row.dataset.itemId = String(i + 1);
    // 行随容器滚动而上移,命中重算才能"看见"新滚入的条目
    row.getBoundingClientRect = () => makeRect(10 + i * 50 - container.scrollTop, 48);
    rows.push(row);
    container.appendChild(row);
  }
  document.body.appendChild(container);

  const ref: RefObject<HTMLDivElement | null> = { current: container };
  const onApply = vi.fn();
  const onPreview = vi.fn();
  const onBackgroundClick = vi.fn();

  const { unmount } = renderHook(() =>
    useRubberBandSelect({
      containerRef: ref,
      enabled: true,
      onApply,
      onPreview,
      onBackgroundClick,
    })
  );

  const mousedown = (clientY: number, clientX = 10) => {
    act(() => {
      rows[0].dispatchEvent(
        new MouseEvent('mousedown', { bubbles: true, cancelable: true, clientX, clientY, button: 0 })
      );
    });
  };
  const mousemove = (clientY: number, clientX = 10) => {
    act(() => {
      window.dispatchEvent(new MouseEvent('mousemove', { clientX, clientY }));
    });
  };
  const mouseup = () => {
    act(() => {
      window.dispatchEvent(new MouseEvent('mouseup', { clientX: 10, clientY: 390 }));
    });
  };
  const frame = () => {
    const pending = [...rafCallbacks.values()];
    rafCallbacks.clear();
    act(() => {
      for (const cb of pending) cb(0);
    });
  };
  const frames = (n: number) => {
    for (let i = 0; i < n; i++) frame();
  };

  return {
    container,
    rows,
    ref,
    onApply,
    onPreview,
    onBackgroundClick,
    mousedown,
    mousemove,
    mouseup,
    unmount,
    frame,
    frames,
  };
}

describe('useRubberBandSelect 框选自动滚动', () => {
  beforeEach(() => {
    rafId = 0;
    rafCallbacks = new Map();
    vi.stubGlobal('requestAnimationFrame', (cb: FrameRequestCallback) => {
      const id = ++rafId;
      rafCallbacks.set(id, cb);
      return id;
    });
    vi.stubGlobal('cancelAnimationFrame', (id: number) => {
      rafCallbacks.delete(id);
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    document.body.innerHTML = '';
  });

  it('拖到底边感应带内时逐帧向下滚动', () => {
    const h = setup();
    h.mousedown(10);
    h.mousemove(390); // bottom(400) - EDGE(28) = 372,390 已在带内

    h.frame(); // 第一帧:命中重算 + 滚动循环启动
    expect(h.container.scrollTop).toBeGreaterThan(0);

    const afterFirst = h.container.scrollTop;
    h.frame();
    expect(h.container.scrollTop).toBeGreaterThan(afterFirst);
    // 单帧位移不超过 MAX_STEP(18)
    expect(afterFirst).toBeLessThanOrEqual(18);
  });

  it('滚动后重新计算命中(onPreview 与 querySelectorAll 再次触发)', () => {
    const h = setup();
    const qsa = vi.spyOn(h.container, 'querySelectorAll');
    h.mousedown(10);
    h.mousemove(390);

    h.frame(); // 首次命中 + 首次滚动
    const qsaAfterFirst = qsa.mock.calls.length;
    const previewAfterFirst = h.onPreview.mock.calls.length;

    h.frame(); // 滚动改变了内容位置 → 必须重算
    expect(h.container.scrollTop).toBeGreaterThan(0);
    expect(qsa.mock.calls.length).toBeGreaterThan(qsaAfterFirst);
    expect(h.onPreview.mock.calls.length).toBeGreaterThan(previewAfterFirst);
  });

  it('松手后停止滚动', () => {
    const h = setup();
    h.mousedown(10);
    h.mousemove(390);
    h.frames(2);

    const during = h.container.scrollTop;
    expect(during).toBeGreaterThan(0);

    h.mouseup();
    h.frames(5);
    expect(h.container.scrollTop).toBe(during);
  });

  it('容器内容不足(scrollHeight <= clientHeight)时不滚动', () => {
    const h = setup({ scrollHeight: 400, clientHeight: 400 });
    h.mousedown(10);
    h.mousemove(390);
    h.frames(5);
    expect(h.container.scrollTop).toBe(0);
  });

  it('光标在容器中部时不动', () => {
    const h = setup();
    h.mousedown(10);
    h.mousemove(200);
    h.frames(5);
    expect(h.container.scrollTop).toBe(0);
  });

  it('向上拖到顶边感应带内时反向滚动', () => {
    const h = setup({ scrollTop: 500 });
    h.mousedown(300);
    h.mousemove(5); // top(0) + EDGE(28) = 28,5 已在带内
    h.frames(2);
    expect(h.container.scrollTop).toBeLessThan(500);
  });

  it('到达底部后不再继续推进', () => {
    const h = setup({ scrollHeight: 1600, clientHeight: 400 }); // 最大 scrollTop = 1200
    h.mousedown(10);
    h.mousemove(390);
    h.frames(200);
    expect(h.container.scrollTop).toBe(1200);
  });

  it('容器卸载(ref 变 null)后不再滚动,也不抛错', () => {
    const h = setup();
    h.mousedown(10);
    h.mousemove(390);
    h.frame();
    const before = h.container.scrollTop;
    expect(before).toBeGreaterThan(0);

    h.ref.current = null;
    expect(() => h.frames(3)).not.toThrow();
    expect(h.container.scrollTop).toBe(before);
  });

  it('hook 卸载时清理滚动循环', () => {
    const h = setup();
    h.mousedown(10);
    h.mousemove(390);
    h.frame();
    const before = h.container.scrollTop;
    expect(before).toBeGreaterThan(0);

    h.unmount();
    expect(rafCallbacks.size).toBe(0);
    h.frames(3);
    expect(h.container.scrollTop).toBe(before);
  });
});
