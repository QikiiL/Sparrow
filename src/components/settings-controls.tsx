import type { ReactNode } from 'react';

/**
 * 设置面板的可复用控件(2026-09 设置面板重设计抽出)。
 * 仅承载视觉与交互样式,不持有业务逻辑;保存语义仍由 SettingsPanel 的
 * saveNow / persistNumbersIfChanged 决定。
 */

interface SwitchProps {
  checked: boolean;
  onChange: () => void;
  label: string;
  disabled?: boolean;
}

/** 统一开关(此前在 SettingsPanel 内联了 6 份相同样式) */
export function Switch({ checked, onChange, label, disabled }: SwitchProps) {
  return (
    <button
      type="button"
      onClick={onChange}
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      className={`relative w-[35px] h-5 rounded-full transition-colors duration-150 flex-shrink-0 ${
        checked ? 'bg-accent' : 'bg-hairline'
      } disabled:opacity-40 disabled:cursor-not-allowed`}
    >
      <span
        className={`absolute top-0.5 left-0.5 w-4 h-4 rounded-full bg-surface shadow-sm transition-transform duration-150 ${
          checked ? 'translate-x-[15px]' : ''
        }`}
      />
    </button>
  );
}

interface StepperProps {
  value: number;
  min?: number;
  ariaLabel: string;
  /** 仅更新本地状态(applySettings),不持久化 */
  onChange: (next: number) => void;
  /** 失焦 / 步进后调用,由调用方判断是否真的需要保存 */
  onCommit: () => void;
}

/** 数字步进器:−/+ 是离散修改直接落盘;中间数字可键盘输入,失焦保存(语义同原数字输入框) */
export function Stepper({ value, min = 0, ariaLabel, onChange, onCommit }: StepperProps) {
  const clamp = (n: number) => Math.max(min, Number.isFinite(n) ? Math.trunc(n) : 0);
  const step = (delta: number) => {
    const next = clamp(value + delta);
    if (next === value) return;
    onChange(next);
    onCommit();
  };
  const btnCls =
    'w-[26px] h-[27px] text-[14px] leading-none text-muted hover:bg-app hover:text-ink transition-colors duration-150';
  return (
    <div className="flex items-center flex-shrink-0 rounded-[9px] border border-hairline overflow-hidden">
      <button type="button" aria-label={`${ariaLabel}减少`} onClick={() => step(-1)} className={btnCls}>
        −
      </button>
      <input
        type="number"
        aria-label={ariaLabel}
        value={value}
        min={min}
        onChange={(e) => onChange(clamp(Number(e.target.value) || 0))}
        onBlur={onCommit}
        className="w-[46px] h-[27px] text-center text-[12px] tabular-nums bg-transparent border-l border-r border-hairline outline-none [appearance:textfield] [&::-webkit-outer-spin-button]:appearance-none [&::-webkit-inner-spin-button]:appearance-none"
      />
      <button type="button" aria-label={`${ariaLabel}增加`} onClick={() => step(1)} className={btnCls}>
        +
      </button>
    </div>
  );
}

interface KeycapsProps {
  /** 修饰键,形如 "Ctrl+Alt" */
  modifier: string;
  /** 主键,形如 "C" */
  keyName: string;
  recording: boolean;
  disabled?: boolean;
  onClick: () => void;
}

/** 键帽式热键展示;点击进入录制(脉冲高亮),disabled 时仅展示不可点 */
export function Keycaps({ modifier, keyName, recording, disabled, onClick }: KeycapsProps) {
  if (recording) {
    return (
      <div className="h-[27px] px-3 flex items-center rounded-[8px] border border-accent bg-accent-soft text-accent text-[11px] animate-pulse select-none cursor-pointer">
        按下快捷键…
      </div>
    );
  }
  const keys = [...modifier.split('+').filter(Boolean), keyName];
  return (
    <div
      onClick={disabled ? undefined : onClick}
      className={`group flex items-center gap-[5px] select-none ${
        disabled ? 'opacity-40 cursor-not-allowed' : 'cursor-pointer'
      }`}
      role="button"
      aria-label="录制快捷键"
    >
      {keys.map((k, i) => (
        <span key={`${k}-${i}`} className="flex items-center gap-[5px]">
          {i > 0 && <span className="text-[11px] text-faint">+</span>}
          <kbd
            className={`min-w-[26px] px-[7px] py-[3px] text-center font-mono text-[11px] text-ink rounded-[6px] bg-app border border-hairline shadow-[0_1.5px_0_var(--color-hairline)] transition-colors duration-150 ${
              disabled ? '' : 'group-hover:border-accent'
            }`}
          >
            {k}
          </kbd>
        </span>
      ))}
    </div>
  );
}

/** 分区小标签(外观 / 通用 / 快捷键 / 隐私 / 数据 / 关于) */
export function SectionLabel({ children }: { children: ReactNode }) {
  return (
    <div className="px-1.5 pt-4 pb-[7px] text-[10.5px] font-medium tracking-[0.14em] text-faint">
      {children}
    </div>
  );
}

/** 分组卡片:每个直接子节点是一个 px-3.5 py-[11px] 的块,块与块之间 hairline 细线分隔 */
export function Card({ children }: { children: ReactNode }) {
  return (
    <div className="rounded-[13px] border border-hairline bg-surface divide-y divide-hairline overflow-hidden">
      {children}
    </div>
  );
}

/** 标准设置行:左侧标题(+一行说明),右侧控件 */
export function Row({
  title,
  desc,
  children,
}: {
  title: ReactNode;
  desc?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="flex items-center justify-between gap-3.5 px-3.5 py-[11px] min-h-[46px]">
      <div className="min-w-0 flex-1">
        <div className="text-[12.5px] font-medium">{title}</div>
        {desc && <div className="mt-[2px] text-[11px] leading-relaxed text-faint">{desc}</div>}
      </div>
      {children}
    </div>
  );
}

/** 幽灵按钮:透明底 + hairline 描边,hover 才浮现底色 */
export const ghostBtn =
  'h-[27px] px-3 text-[11px] rounded-[8px] border border-hairline bg-transparent text-muted hover:bg-app hover:text-ink transition-colors duration-150 disabled:opacity-40 disabled:hover:bg-transparent disabled:hover:text-muted disabled:cursor-not-allowed';

/** 危险操作:红色文字按钮,hover 淡红底 */
export const ghostBtnDanger =
  'h-[27px] px-3 text-[11px] rounded-[8px] border border-transparent bg-transparent text-danger hover:bg-danger/10 transition-colors duration-150 disabled:opacity-40 disabled:cursor-not-allowed';

/** 方形图标幽灵按钮(如重置热键) */
export const ghostBtnIcon =
  'flex h-[27px] w-[27px] flex-shrink-0 items-center justify-center rounded-[8px] border border-hairline bg-transparent text-muted hover:bg-app hover:text-ink transition-colors duration-150 disabled:opacity-40 disabled:cursor-not-allowed';

interface TextInputProps {
  value: string;
  onChange: (next: string) => void;
  placeholder?: string;
  type?: 'text' | 'password';
  disabled?: boolean;
  className?: string;
  /** 失焦时提交(文本类设置按失焦落盘,避免每敲一个字符就写一次配置) */
  onBlur?: () => void;
}

/** 单行文本输入(云同步等分区用) */
export function TextInput({
  value,
  onChange,
  placeholder,
  type = 'text',
  disabled,
  className = '',
  onBlur,
}: TextInputProps) {
  return (
    <input
      type={type}
      value={value}
      placeholder={placeholder}
      disabled={disabled}
      onChange={(e) => onChange(e.target.value)}
      onBlur={onBlur}
      className={`bg-app border border-hairline rounded-[9px] px-2.5 py-1.5 text-[12.5px] text-ink placeholder:text-faint outline-none transition-[border-color,box-shadow] duration-150 focus:outline-none focus:ring-1 focus:ring-accent/40 disabled:opacity-40 disabled:cursor-not-allowed ${className}`}
    />
  );
}

interface SegmentedProps<T extends string> {
  value: T;
  options: { value: T; label: string }[];
  onChange: (next: T) => void;
}

/** 分段切换(与设置面板「关闭行为」同款样式) */
export function Segmented<T extends string>({ value, options, onChange }: SegmentedProps<T>) {
  return (
    <div className="flex gap-[3px] p-0.5 rounded-[9px] bg-app border border-hairline">
      {options.map((opt) => (
        <button
          key={opt.value}
          type="button"
          onClick={() => onChange(opt.value)}
          aria-pressed={value === opt.value}
          className={`flex-1 h-[26px] px-2 text-[11.5px] rounded-[7px] whitespace-nowrap transition-[background-color,color,box-shadow] duration-150 ${
            value === opt.value
              ? 'bg-surface text-accent font-semibold shadow-sm'
              : 'text-muted hover:text-faint'
          }`}
        >
          {opt.label}
        </button>
      ))}
    </div>
  );
}

/** 状态圆点(8px):ok / warn / danger / idle */
export function StatusDot({ tone }: { tone: 'ok' | 'warn' | 'danger' | 'idle' }) {
  const bg = {
    ok: 'bg-ok',
    warn: 'bg-warn',
    danger: 'bg-danger',
    idle: 'bg-faint',
  }[tone];
  return <span aria-hidden="true" className={`w-2 h-2 rounded-full flex-shrink-0 ${bg}`} />;
}
