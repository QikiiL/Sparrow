import { useState, useEffect, useCallback, useRef } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { getVersion } from '@tauri-apps/api/app';
import { useTheme } from '../contexts/ThemeContext';
import { useClipboardStore } from '../stores/clipboardStore';
import type { AppSettings } from '../types/settings';
import { DEFAULT_SETTINGS } from '../types/settings';
import { XIcon, ResetIcon } from './icons';
import { PromptDialog } from './Dialogs';
import { SyncSection } from './SyncSection';
import { parseUpdateNotes, type UpdateInfo } from './UpdateDialog';
import { openUrl } from '../lib/openUrl';
import { UI_STYLES } from '../lib/uiStyle';
import {
  Switch,
  Stepper,
  Keycaps,
  SectionLabel,
  Card,
  Row,
  ghostBtn,
  ghostBtnDanger,
  ghostBtnIcon,
} from './settings-controls';

interface Props {
  isOpen: boolean;
  onClose: () => void;
}

interface StorageInfo {
  data_dir: string;
  is_default: boolean;
  default_dir: string;
}

interface SmsCodeStatus {
  enabled: boolean;
  /** allowed / denied / unspecified / unsupported */
  access: string;
  last_capture: number;
  capture_count: number;
}

// 清除范围选项:days 为 0 表示全部,>0 表示清除 N 天前(含更早)的记录
const CLEAR_RANGES = [
  { days: 0, label: '全部' },
  { days: 90, label: '三个月前' },
  { days: 30, label: '一个月前' },
  { days: 7, label: '七天前' },
  { days: 3, label: '三天前' },
] as const;

// Unix 秒 → 「14:32」时间文本;0 表示本次会话还没有捕获
function formatCaptureTime(unix: number): string {
  if (unix <= 0) return '刚开启';
  return new Date(unix * 1000).toLocaleTimeString('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
  });
}

export function SettingsPanel({ isOpen, onClose }: Props) {
  const [settings, setSettings] = useState<AppSettings>(DEFAULT_SETTINGS);
  // 正在录制哪一组热键:null = 未录制;hotkey = 唤出热键,seq_paste = 连续粘贴热键
  type HotkeyField = 'hotkey' | 'seq_paste';
  const [recording, setRecording] = useState<HotkeyField | null>(null);
  const [winVEnabled, setWinVEnabled] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [clearConfirmOpen, setClearConfirmOpen] = useState(false);
  const [clearDays, setClearDays] = useState(0);
  const [storageInfo, setStorageInfo] = useState<StorageInfo | null>(null);
  const [updateChecking, setUpdateChecking] = useState(false);
  const [updateResult, setUpdateResult] = useState<UpdateInfo | null>(null);
  const [updateError, setUpdateError] = useState<string | null>(null);
  const [pwdCopied, setPwdCopied] = useState(false);
  const [addingApp, setAddingApp] = useState(false);
  const [addingPattern, setAddingPattern] = useState(false);
  // 短信验证码功能状态(权限/最近捕获),来自后端轮询
  const [smsStatus, setSmsStatus] = useState<SmsCodeStatus | null>(null);
  // 标题右侧版本徽章
  const [appVersion, setAppVersion] = useState<string | null>(null);
  // settingsRef 始终持有最新设置,避免回调闭包读到旧值
  const settingsRef = useRef<AppSettings>(DEFAULT_SETTINGS);
  // savedRef 持有最近一次已持久化的值,用于判断数字输入是否真的改了
  const savedRef = useRef<AppSettings>(DEFAULT_SETTINGS);
  const recordingRef = useRef<HotkeyField | null>(null);
  const { theme, toggleTheme, style, setStyle } = useTheme();
  const setPaused = useClipboardStore((s) => s.setPaused);

  // 豁免名单条数。后端 AppSettings 对该字段带 serde default,
  // 老配置文件读回来也是空数组,这里可以直接取 length
  const allowlistCount = settings.excluded_allowlist.length;

  const applySettings = useCallback((next: AppSettings) => {
    settingsRef.current = next;
    setSettings(next);
  }, []);

  useEffect(() => {
    if (isOpen) {
      invoke<AppSettings>('load_settings').then((s) => {
        // 与默认值合并,避免旧配置缺字段导致保存时丢失(如 pinned)
        const merged = { ...DEFAULT_SETTINGS, ...s };
        applySettings(merged);
        savedRef.current = merged;
        setPaused(s.paused);
        setWinVEnabled(s.win_v_integration ?? false);
      }).catch(console.error);
      setRecording(null);
      recordingRef.current = null;
      setClearConfirmOpen(false);
      setClearDays(0);
      invoke<StorageInfo>('get_storage_info')
        .then(setStorageInfo)
        .catch(console.error);
      getVersion()
        .then(setAppVersion)
        .catch(() => {});
    }
  }, [isOpen, setPaused, applySettings]);

  // 短信验证码状态:面板打开时加载,并每 3 秒刷新——用户被引导去
  // 系统设置里授权,回来后状态行要能自动变成「已开启」,不需要手动刷新
  const refreshSmsStatus = useCallback(() => {
    invoke<SmsCodeStatus>('sms_code_status')
      .then(setSmsStatus)
      .catch(console.error);
  }, []);

  useEffect(() => {
    if (!isOpen) return;
    refreshSmsStatus();
    const timer = setInterval(refreshSmsStatus, 3000);
    return () => clearInterval(timer);
  }, [isOpen, refreshSmsStatus]);

  const handleToggleSmsCode = async () => {
    // 与其他开关一致读 settingsRef:settings state 可能滞后于最近一次保存
    const next = !settingsRef.current.sms_code_enabled;
    await saveNow({ ...settingsRef.current, sms_code_enabled: next });
    if (next) {
      // 打开时请求一次权限:非打包应用不弹系统对话框,这个调用的作用
      // 是把本应用注册进系统设置的名单;真正放行仍需用户手动勾选
      try {
        await invoke<string>('sms_code_request_access');
      } catch (err) {
        console.error('Failed to request notification access:', err);
      }
      refreshSmsStatus();
    }
  };

  const handleOpenNotificationSettings = async () => {
    try {
      await invoke('open_notification_settings');
    } catch (err) {
      console.error('Failed to open notification settings:', err);
    }
  };

  useEffect(() => {
    if (!isOpen) return;
    const handleEscape = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        if (recordingRef.current) {
          setRecording(null);
          recordingRef.current = null;
        } else if (clearConfirmOpen) {
          setClearConfirmOpen(false);
        } else {
          onClose();
        }
      }
    };
    document.addEventListener('keydown', handleEscape);
    return () => document.removeEventListener('keydown', handleEscape);
  }, [isOpen, onClose, clearConfirmOpen]);

  // 即改即存:应用新值并持久化;paused 变化时同步监听开关
  const saveNow = useCallback(async (next: AppSettings) => {
    const prev = settingsRef.current;
    applySettings(next);
    try {
      await invoke('save_settings', { settings: next });
      savedRef.current = next;
      if (prev.paused !== next.paused) {
        await invoke('pause_monitoring', { paused: next.paused });
        setPaused(next.paused);
      }
      // 列表查询上限跟随设置(0=不限制)
      useClipboardStore
        .getState()
        .setMaxItems(next.max_item_count > 0 ? next.max_item_count : 100000);
    } catch (err) {
      console.error('Auto-save settings failed:', err);
    }
  }, [applySettings, setPaused]);

  // 热键类修改需先注册成功再保存,失败则回读后端恢复原值
  const saveHotkey = useCallback(async (next: AppSettings) => {
    applySettings(next);
    try {
      await invoke('register_hotkey', {
        modifier: next.hotkey_modifier,
        key: next.hotkey_key,
      });
      await saveNow(next);
    } catch (err) {
      console.error('Register hotkey failed:', err);
      alert('快捷键注册失败: ' + err);
      invoke<AppSettings>('load_settings')
        .then((s) => {
          const merged = { ...DEFAULT_SETTINGS, ...s };
          applySettings(merged);
          savedRef.current = merged;
        })
        .catch(console.error);
    }
  }, [applySettings, saveNow]);

  // Hotkey recording handler(唤出热键与连续粘贴热键共用录制逻辑)
  const handleRecordKeyDown = useCallback((e: KeyboardEvent) => {
    if (!recordingRef.current) return;
    e.preventDefault();
    e.stopPropagation();

    const modifiers: string[] = [];
    if (e.ctrlKey) modifiers.push('Ctrl');
    if (e.altKey) modifiers.push('Alt');
    if (e.shiftKey) modifiers.push('Shift');
    if (e.metaKey) modifiers.push('Super');

    const modifierKeys = ['Control', 'Alt', 'Shift', 'Meta'];
    if (modifierKeys.includes(e.key)) return;

    // Only accept single letter keys (A-Z) and digits (0-9)
    const isValidKey = /^[a-zA-Z0-9]$/.test(e.key);
    if (!isValidKey) return;

    // 裸键(无修饰键)会注册成全局单键热键,单按一个字母就唤出面板,必须拦下;
    // 不结束录制状态,让用户补按修饰键
    if (modifiers.length === 0) {
      alert('请至少搭配一个修饰键（Ctrl / Alt / Shift / Win）');
      return;
    }

    const modifier = modifiers.join('+');
    const mainKey = e.key.toUpperCase();
    const isSeq = recordingRef.current === 'seq_paste';

    // 连续粘贴热键与唤出热键相同会导致两个 handler 抢同一个全局组合键
    if (isSeq && modifier === settingsRef.current.hotkey_modifier && mainKey === settingsRef.current.hotkey_key) {
      alert('连续粘贴热键不能与唤出窗口的全局热键相同');
      setRecording(null);
      recordingRef.current = null;
      return;
    }

    const next = isSeq
      ? { ...settingsRef.current, seq_paste_modifier: modifier, seq_paste_key: mainKey }
      : { ...settingsRef.current, hotkey_modifier: modifier, hotkey_key: mainKey };
    setRecording(null);
    recordingRef.current = null;
    // 唤出热键需立即注册验证;连续粘贴热键只在队列开始时才注册,直接保存即可
    if (isSeq) {
      void saveNow(next);
    } else {
      void saveHotkey(next);
    }
  }, [saveHotkey, saveNow]);

  useEffect(() => {
    if (!recording) return;
    document.addEventListener('keydown', handleRecordKeyDown);
    return () => document.removeEventListener('keydown', handleRecordKeyDown);
  }, [recording, handleRecordKeyDown]);

  const startRecording = () => {
    setRecording('hotkey');
    recordingRef.current = 'hotkey';
  };

  const startRecordingSeq = () => {
    setRecording('seq_paste');
    recordingRef.current = 'seq_paste';
  };

  const resetHotkey = () => {
    void saveHotkey({
      ...settingsRef.current,
      hotkey_modifier: DEFAULT_SETTINGS.hotkey_modifier,
      hotkey_key: DEFAULT_SETTINGS.hotkey_key,
    });
  };

  const resetSeqHotkey = () => {
    void saveNow({
      ...settingsRef.current,
      seq_paste_modifier: DEFAULT_SETTINGS.seq_paste_modifier,
      seq_paste_key: DEFAULT_SETTINGS.seq_paste_key,
    });
  };

  const handleToggleWinV = async () => {
    const next = !winVEnabled;
    try {
      if (next) {
        await invoke('enable_win_v_integration');
      } else {
        await invoke('disable_win_v_integration');
      }
      setWinVEnabled(next);
      // 后端已持久化该字段,同步本地镜像避免下次保存时覆盖
      applySettings({ ...settingsRef.current, win_v_integration: next });
    } catch (err) {
      console.error('[Win+V] Toggle failed:', err);
      alert('Win+V 切换失败: ' + err);
    }
  };

  const handleToggleAutostart = () => {
    void saveNow({ ...settingsRef.current, start_with_windows: !settingsRef.current.start_with_windows });
  };

  const handleTogglePaused = () => {
    void saveNow({ ...settingsRef.current, paused: !settingsRef.current.paused });
  };

  const setCloseBehavior = (value: AppSettings['close_behavior']) => {
    void saveNow({ ...settingsRef.current, close_behavior: value });
  };

  // 数字输入:失焦时若值有变则保存
  const persistNumbersIfChanged = () => {
    if (settingsRef.current !== savedRef.current) {
      void saveNow(settingsRef.current);
    }
  };

  const doClearHistory = useCallback(async () => {
    setClearConfirmOpen(false);
    try {
      setClearing(true);
      await invoke('clear_history', { days: clearDays });
    } catch (err) {
      console.error('Clear history failed:', err);
      alert('清空失败: ' + err);
    } finally {
      setClearing(false);
    }
  }, [clearDays]);

  // 更改/恢复存储位置:后端选目录+校验+写指针后重启,冷启动完成数据搬运
  const handleChangeStorage = async () => {
    if (
      !window.confirm(
        '更改存储位置将迁移数据库与图片,迁移完成后应用会自动重启。所选文件夹必须是空文件夹。继续?'
      )
    ) {
      return;
    }
    try {
      await invoke('change_storage_location');
    } catch (err) {
      alert('更改存储位置失败: ' + err);
    }
  };

  const handleResetStorage = async () => {
    if (!window.confirm('把数据迁回默认位置并重启应用,继续?')) return;
    try {
      await invoke('reset_storage_location');
    } catch (err) {
      alert('恢复默认位置失败: ' + err);
    }
  };

  // 手动检查更新:结果内联展示,下载按钮打开浏览器
  const handleCheckUpdate = async () => {
    setUpdateChecking(true);
    setUpdateError(null);
    setUpdateResult(null);
    try {
      const info = await invoke<UpdateInfo>('check_update');
      setUpdateResult(info);
    } catch (err) {
      setUpdateError(String(err));
    } finally {
      setUpdateChecking(false);
    }
  };

  // 与 UpdateDialog 相同的双保险:点击下载自动复制密码 + 明文展示可手动复制
  const copyUpdatePassword = async () => {
    if (!updateResult?.lanzou_password) return;
    try {
      await invoke('write_clipboard_text', { text: updateResult.lanzou_password });
      setPwdCopied(true);
      setTimeout(() => setPwdCopied(false), 2000);
    } catch (err) {
      console.error('Copy password failed:', err);
    }
  };

  // 渲染时只解析一次(此前在 JSX 里调用了两遍)
  const updateNoteLines = updateResult ? parseUpdateNotes(updateResult.notes) : [];

  if (!isOpen) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50">
      <div className="clip-dialog bg-surface rounded-[14px] shadow-dialog border border-hairline w-full max-w-md mx-4 max-h-[90vh] flex flex-col overflow-hidden">
        <div className="flex items-center justify-between px-[18px] pt-[15px] pb-[13px]">
          <h2 className="text-[14.5px] font-semibold tracking-[0.01em]">
            设置
            {appVersion && (
              <span className="ml-2 px-2 py-[2px] rounded-full border border-hairline text-[10.5px] font-normal text-faint align-[1px]">
                v{appVersion}
              </span>
            )}
          </h2>
          <button
            onClick={onClose}
            className="flex items-center justify-center w-[26px] h-[26px] rounded-lg text-faint hover:bg-app hover:text-muted transition-colors"
          >
            <XIcon size={13} />
          </button>
        </div>

        <div className="px-4 pb-1 overflow-y-auto">
          {/* 外观 */}
          <SectionLabel>外观</SectionLabel>
          <Card>
            <Row title="深色模式">
              <Switch checked={theme === 'dark'} onChange={toggleTheme} label="深色模式" />
            </Row>
            {/* 界面风格:即选即生效(只改 state,effect 写 dataset + localStorage,不调 invoke) */}
            <div className="px-3.5 py-[11px]">
              <div className="text-[12.5px] font-medium">界面风格</div>
              <p className="mt-[2px] text-[11px] text-faint">即选即生效；浮笺为默认</p>
              <div
                role="radiogroup"
                aria-label="界面风格"
                className="flex mt-2 gap-[3px] p-[3px] rounded-[10px] bg-app"
              >
                {UI_STYLES.map((s) => (
                  <button
                    key={s.value}
                    role="radio"
                    aria-checked={style === s.value}
                    onClick={() => setStyle(s.value)}
                    className={`flex-1 h-[26px] text-[11.5px] rounded-[7px] transition-[background-color,color,box-shadow] duration-150 ${
                      style === s.value
                        ? 'bg-surface text-accent font-semibold shadow-lift'
                        : 'text-muted hover:text-faint'
                    }`}
                  >
                    {s.label}
                  </button>
                ))}
              </div>
            </div>
          </Card>

          {/* 通用 */}
          <SectionLabel>通用</SectionLabel>
          <Card>
            <Row title="开机自启动">
              <Switch
                checked={settings.start_with_windows}
                onChange={handleToggleAutostart}
                label="开机自启动"
              />
            </Row>
            <Row title="暂停监听" desc="开启后复制的内容不再记录">
              <Switch checked={settings.paused} onChange={handleTogglePaused} label="暂停监听" />
            </Row>
            <Row title="历史保留天数" desc="0 表示永久保留">
              <Stepper
                value={settings.retention_days}
                ariaLabel="历史保留天数"
                onChange={(n) => applySettings({ ...settingsRef.current, retention_days: n })}
                onCommit={persistNumbersIfChanged}
              />
            </Row>
            <Row title="最大存储条数" desc="0 表示无限制">
              <Stepper
                value={settings.max_item_count}
                ariaLabel="最大存储条数"
                onChange={(n) => applySettings({ ...settingsRef.current, max_item_count: n })}
                onCommit={persistNumbersIfChanged}
              />
            </Row>
            <div className="px-3.5 py-[11px]">
              <div className="text-[12.5px] font-medium">关闭行为</div>
              <div className="flex mt-2 gap-[3px] p-[3px] rounded-[10px] bg-app">
                {([
                  { value: 'ask', label: '询问' },
                  { value: 'minimize', label: '最小化到托盘' },
                  { value: 'close', label: '直接关闭' },
                ] as const).map((opt) => (
                  <button
                    key={opt.value}
                    onClick={() => setCloseBehavior(opt.value)}
                    className={`flex-1 h-[26px] text-[11.5px] rounded-[7px] transition-[background-color,color,box-shadow] duration-150 ${
                      settings.close_behavior === opt.value
                        ? 'bg-surface text-accent font-semibold shadow-lift'
                        : 'text-muted hover:text-faint'
                    }`}
                  >
                    {opt.label}
                  </button>
                ))}
              </div>
            </div>
          </Card>

          {/* 快捷键 */}
          <SectionLabel>快捷键</SectionLabel>
          <Card>
            <Row
              title="唤出窗口"
              desc={
                winVEnabled
                  ? '已停用（Win+V 模式）'
                  : recording === 'hotkey'
                    ? '按下想要的快捷键组合…'
                    : '点击键帽重新录制，录制完成即生效'
              }
            >
              <div className="flex items-center gap-1.5">
                <Keycaps
                  modifier={settings.hotkey_modifier}
                  keyName={settings.hotkey_key}
                  recording={recording === 'hotkey'}
                  disabled={winVEnabled}
                  onClick={startRecording}
                />
                <button
                  onClick={resetHotkey}
                  disabled={winVEnabled}
                  title="恢复默认快捷键"
                  className={ghostBtnIcon}
                >
                  <ResetIcon size={13} />
                </button>
              </div>
            </Row>
            <Row
              title="连续粘贴"
              desc={
                recording === 'seq_paste'
                  ? '按下想要的快捷键组合…'
                  : '队列进行中按一次贴一条，不能与唤出热键相同'
              }
            >
              <div className="flex items-center gap-1.5">
                <Keycaps
                  modifier={settings.seq_paste_modifier}
                  keyName={settings.seq_paste_key}
                  recording={recording === 'seq_paste'}
                  onClick={startRecordingSeq}
                />
                <button onClick={resetSeqHotkey} title="恢复默认快捷键" className={ghostBtnIcon}>
                  <ResetIcon size={13} />
                </button>
              </div>
            </Row>
            <Row
              title="替代系统 Win+V"
              desc={
                winVEnabled ? (
                  <span className="text-warn-text">已启用 Win+V，自定义唤出热键暂时停用</span>
                ) : (
                  '禁用系统剪贴板历史，用本应用替代'
                )
              }
            >
              <Switch
                checked={winVEnabled}
                onChange={() => void handleToggleWinV()}
                label="Win+V 替代系统剪贴板"
              />
            </Row>
          </Card>

          {/* 隐私 */}
          <SectionLabel>隐私</SectionLabel>
          <Card>
            <Row
              title="自动识别密钥 / 卡号"
              desc="命中即不记录；误伤可在底部提示条点「仍要记录」找回"
            >
              <Switch
                checked={settings.detect_sensitive}
                onChange={() =>
                  void saveNow({
                    ...settingsRef.current,
                    detect_sensitive: !settingsRef.current.detect_sensitive,
                  })
                }
                label="自动识别密钥与卡号"
              />
            </Row>

            {/* 来源软件 */}
            <div className="px-3.5 py-[11px]">
              <div className="flex items-center justify-between gap-3">
                <div className="text-[12.5px] font-medium">来源软件</div>
                <button onClick={() => setAddingApp(true)} className={ghostBtn}>
                  添加
                </button>
              </div>
              <p
                className="mt-[2px] text-[11px] text-faint"
                title="填软件的文件名，例如 keepass.exe。注意：如果复制完立刻把这个软件关了，就认不出是哪个软件，这条规则会失效"
              >
                从这些软件复制的内容一律不记录
              </p>
              <div className="flex flex-wrap gap-1.5 mt-2">
                {settings.excluded_apps.map((app) => (
                  <span
                    key={app}
                    className="flex items-center gap-1 pl-2.5 pr-1.5 py-[3px] text-[11px] rounded-full bg-app border border-hairline text-muted"
                  >
                    <span className="font-mono">{app}</span>
                    <button
                      onClick={() =>
                        void saveNow({
                          ...settingsRef.current,
                          excluded_apps: settings.excluded_apps.filter((a) => a !== app),
                        })
                      }
                      className="flex items-center text-faint hover:text-danger transition-colors"
                      title="移除"
                    >
                      <XIcon size={11} />
                    </button>
                  </span>
                ))}
                {settings.excluded_apps.length === 0 && (
                  <p className="text-[11px] text-faint">还没添加，所有软件复制的内容都会记录</p>
                )}
              </div>
            </div>

            {/* 内容匹配规则 */}
            <div className="px-3.5 py-[11px]">
              <div className="flex items-center justify-between gap-3">
                <div className="text-[12.5px] font-medium">内容匹配规则</div>
                <button onClick={() => setAddingPattern(true)} className={ghostBtn}>
                  添加
                </button>
              </div>
              <p
                className="mt-[2px] text-[11px] text-faint"
                title="可以直接写字（如：内部机密），也可以写带格式的规则（如：订单号[0-9]+ 表示「订单号」后面跟一串数字）。填错了不会报错，只会跳过这一条"
              >
                内容里出现设定文字就不记录，支持正则
              </p>
              <div className="mt-2 space-y-1">
                {settings.excluded_patterns.map((pattern) => (
                  <div
                    key={pattern}
                    className="flex items-center gap-1.5 px-2 py-1 rounded-md bg-app border border-hairline"
                  >
                    <span className="flex-1 min-w-0 text-[11px] font-mono text-muted truncate" title={pattern}>
                      {pattern}
                    </span>
                    <button
                      onClick={() =>
                        void saveNow({
                          ...settingsRef.current,
                          excluded_patterns: settings.excluded_patterns.filter((p) => p !== pattern),
                        })
                      }
                      className="flex-shrink-0 flex items-center text-faint hover:text-danger transition-colors"
                      title="移除"
                    >
                      <XIcon size={11} />
                    </button>
                  </div>
                ))}
                {settings.excluded_patterns.length === 0 && (
                  <p className="text-[11px] text-faint">还没添加。可以填一段文字，如：内部机密</p>
                )}
              </div>
            </div>

            <Row
              title="豁免名单"
              desc={
                allowlistCount === 0
                  ? '点过「仍要记录」的内容会出现在这里'
                  : `已豁免 ${allowlistCount} 条，以后正常记录不再拦截`
              }
            >
              <button
                onClick={() => void saveNow({ ...settingsRef.current, excluded_allowlist: [] })}
                disabled={allowlistCount === 0}
                className={ghostBtn}
              >
                清空
              </button>
            </Row>

            {/* 短信验证码 */}
            <div className="px-3.5 py-[11px]">
              <div className="flex items-center justify-between gap-3">
                <div className="min-w-0 flex-1">
                  <div className="text-[12.5px] font-medium">短信验证码自动复制</div>
                  <div
                    className="mt-[2px] text-[11px] text-faint"
                    title="需要先在电脑的「手机连接」里配对手机；短信内容只在电脑本地处理，不会上传"
                  >
                    手机收到验证码短信时自动复制到剪贴板
                  </div>
                </div>
                <Switch
                  checked={settings.sms_code_enabled}
                  onChange={() => void handleToggleSmsCode()}
                  label="自动复制短信验证码"
                />
              </div>

              {/* 权限状态行:3 秒自动刷新,用户去系统设置勾选后回来就能看到变化 */}
              {settings.sms_code_enabled && smsStatus && (
                <div className="mt-2 flex items-start justify-between gap-3">
                  <p className="text-[11px] text-faint min-w-0 flex-1 leading-relaxed">
                    {smsStatus.access === 'allowed' && (
                      <>
                        <span className="inline-block w-1.5 h-1.5 rounded-full bg-ok mr-1.5 align-[1px]" />
                        通知权限已开启
                        {smsStatus.capture_count > 0 && (
                          <>，本次已捕获 {smsStatus.capture_count} 个（最近 {formatCaptureTime(smsStatus.last_capture)}）</>
                        )}
                      </>
                    )}
                    {smsStatus.access === 'unsupported' && '此系统版本不支持读取通知，功能不可用'}
                    {(smsStatus.access === 'denied' || smsStatus.access === 'unspecified') && (
                      <>还没获得读取通知的权限。点右侧按钮打开系统设置，在「允许应用访问通知」里勾选本应用（剪雀 Sparrow）</>
                    )}
                  </p>
                  {(smsStatus.access === 'denied' || smsStatus.access === 'unspecified') && (
                    <button
                      onClick={() => void handleOpenNotificationSettings()}
                      className={`${ghostBtn} flex-shrink-0`}
                    >
                      打开系统设置
                    </button>
                  )}
                </div>
              )}
              {!settings.sms_code_enabled && (
                <p className="mt-1.5 text-[11px] text-faint">
                  打开后会请求读取 Windows 通知的权限，内容仅在本地处理
                </p>
              )}
            </div>
          </Card>

          {/* 云同步 */}
          <SyncSection settings={settings} saveNow={saveNow} />

          {/* 数据 */}
          <SectionLabel>数据</SectionLabel>
          <Card>
            <div className="px-3.5 py-[11px]">
              <div className="flex items-center justify-between gap-3">
                <div className="text-[12.5px] font-medium">数据存储位置</div>
                <div className="flex gap-2 flex-shrink-0">
                  {storageInfo && !storageInfo.is_default && (
                    <button onClick={handleResetStorage} className={ghostBtn}>
                      恢复默认
                    </button>
                  )}
                  <button onClick={handleChangeStorage} className={ghostBtn}>
                    更改
                  </button>
                </div>
              </div>
              <p
                className="mt-[5px] text-[10.5px] font-mono text-faint truncate"
                title="数据库与图片保存在此目录，迁移在重启后进行且失败自动回退"
              >
                {storageInfo ? storageInfo.data_dir : '加载中…'}
              </p>
            </div>
            <Row title="清空剪贴板历史" desc="按时间范围清除未收藏的记录及图片，收藏保留">
              <button
                onClick={() => setClearConfirmOpen(true)}
                disabled={clearing}
                className={ghostBtnDanger}
              >
                {clearing ? '清空中…' : '清空…'}
              </button>
            </Row>
          </Card>

          {/* 关于 */}
          <SectionLabel>关于</SectionLabel>
          <Card>
            <div className="px-3.5 py-[11px]">
              <div className="flex items-center justify-between gap-3">
                <div className="text-[12.5px] font-medium">软件更新</div>
                <button onClick={handleCheckUpdate} disabled={updateChecking} className={ghostBtn}>
                  {updateChecking ? '检查中…' : '检查更新'}
                </button>
              </div>
              {updateResult && !updateResult.has_update && (
                <p className="mt-1.5 text-[11px] text-faint">已是最新版本（v{updateResult.current}）</p>
              )}
              {updateResult && updateResult.has_update && (
                <div className="mt-2">
                  <p className="text-[11px] text-faint">
                    当前 v{updateResult.current} → 最新 <span className="text-accent">v{updateResult.latest}</span>
                  </p>
                  {updateNoteLines.length > 0 && (
                    <ul className="mt-1.5 space-y-1">
                      {updateNoteLines.map((line, i) => (
                        <li key={i} className="flex gap-1.5 text-[11px] leading-relaxed text-faint">
                          <span className="text-accent shrink-0 select-none">•</span>
                          <span className="min-w-0 break-words">{line}</span>
                        </li>
                      ))}
                    </ul>
                  )}
                  {updateResult.lanzou && updateResult.lanzou_password && (
                    <div className="flex items-center gap-2 mt-2 px-3 py-2 rounded-[8px] bg-app border border-hairline">
                      <span className="text-[11px] text-faint shrink-0">蓝奏云密码</span>
                      <span className="text-[13px] font-mono font-medium tracking-widest text-ink select-all truncate">
                        {updateResult.lanzou_password}
                      </span>
                      <button
                        onClick={copyUpdatePassword}
                        className={`${ghostBtn} ml-auto shrink-0 h-[22px] px-2.5 rounded-[6px]`}
                      >
                        {pwdCopied ? '已复制' : '复制'}
                      </button>
                    </div>
                  )}
                  <div className="flex gap-2 mt-2">
                    {updateResult.lanzou && (
                      <button
                        onClick={() => {
                          if (updateResult.lanzou_password) void copyUpdatePassword();
                          openUrl(updateResult.lanzou!);
                        }}
                        className={ghostBtn}
                      >
                        蓝奏云下载
                      </button>
                    )}
                    {updateResult.github && (
                      <button onClick={() => openUrl(updateResult.github!)} className={ghostBtn}>
                        GitHub 下载
                      </button>
                    )}
                  </div>
                </div>
              )}
              {updateError && (
                <p className="mt-1.5 text-[11px] text-faint">{updateError}（无网络或版本服务不可用）</p>
              )}
            </div>
          </Card>

          <div className="py-3 text-center text-[10.5px] tracking-[0.04em] text-faint">
            更改即时保存
          </div>
        </div>
      </div>

      {/* 清空历史确认弹窗(居中) */}
      {clearConfirmOpen && (
        <div className="fixed inset-0 z-[60] flex items-center justify-center bg-black/50">
          <div className="clip-dialog bg-surface rounded-[14px] shadow-dialog border border-hairline w-full max-w-xs mx-4 overflow-hidden">
            <div className="px-5 pt-3.5 pb-3 border-b border-hairline">
              <h3 className="text-sm font-semibold">清空剪贴板历史</h3>
            </div>
            <div className="px-5 py-4">
              <p className="text-[12px] font-medium mb-2">清除范围</p>
              <div className="flex gap-[3px] p-[3px] rounded-[10px] bg-app">
                {CLEAR_RANGES.map((opt) => (
                  <button
                    key={opt.days}
                    onClick={() => setClearDays(opt.days)}
                    className={`flex-1 h-[26px] text-[11px] rounded-[7px] transition-[background-color,color,box-shadow] duration-150 ${
                      clearDays === opt.days
                        ? 'bg-surface text-accent font-semibold shadow-lift'
                        : 'text-muted hover:text-faint'
                    }`}
                  >
                    {opt.label}
                  </button>
                ))}
              </div>
              <p className="text-[12.5px] leading-relaxed text-ink mt-3">
                {clearDays === 0
                  ? '将删除除收藏外的全部记录及其图片文件。'
                  : `将删除"${CLEAR_RANGES.find((r) => r.days === clearDays)?.label}"及更早的未收藏记录及其图片文件。`}
                此操作不可恢复,收藏的条目(含图片)会保留。
              </p>
            </div>
            <div className="flex justify-end gap-2 px-5 pt-3 pb-4 border-t border-hairline">
              <button
                onClick={() => setClearConfirmOpen(false)}
                className="h-[31px] px-4 text-[12.5px] rounded-[10px] text-muted hover:bg-app transition-colors duration-150"
              >
                取消
              </button>
              <button
                onClick={doClearHistory}
                className="h-[31px] px-4 text-[12.5px] rounded-[10px] bg-danger text-on-accent font-medium hover:bg-danger-deep transition-colors duration-150"
              >
                清空
              </button>
            </div>
          </div>
        </div>
      )}

      {/* 添加排除进程 */}
      {addingApp && (
        <PromptDialog
          title="添加来源软件"
          label="软件的文件名，例如 keepass.exe："
          placeholder="keepass.exe"
          confirmText="添加"
          onConfirm={async (name) => {
            const value = name.trim().toLowerCase();
            if (settingsRef.current.excluded_apps.includes(value)) {
              throw new Error('这个软件已经在列表里了');
            }
            await saveNow({
              ...settingsRef.current,
              excluded_apps: [...settingsRef.current.excluded_apps, value],
            });
            setAddingApp(false);
          }}
          onClose={() => setAddingApp(false)}
        />
      )}

      {/* 添加排除正则 */}
      {addingPattern && (
        <PromptDialog
          title="添加匹配规则"
          label="内容里出现什么就不记录："
          placeholder="内部机密"
          confirmText="添加"
          maxLength={200}
          onConfirm={async (pattern) => {
            // 先做一次尽力而为的前端预检,把括号不匹配之类的常见手误就地拦下。
            // 注意 JS RegExp 与 Rust regex 语法并不等价(JS 支持前瞻、反向引用,
            // Rust 不支持),因此这里通过不代表后端一定接受——最终仍以 Rust 侧
            // 编译为准,编译失败的条目会被跳过并记日志,不会 panic
            try {
              new RegExp(pattern);
            } catch (err) {
              throw new Error('这条规则可能写错了：' + String(err));
            }
            if (settingsRef.current.excluded_patterns.includes(pattern)) {
              throw new Error('这条规则已经在列表里了');
            }
            await saveNow({
              ...settingsRef.current,
              excluded_patterns: [...settingsRef.current.excluded_patterns, pattern],
            });
            setAddingPattern(false);
          }}
          onClose={() => setAddingPattern(false)}
        />
      )}
    </div>
  );
}
