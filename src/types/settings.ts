import type { SyncSettings } from './sync';

export type CloseBehavior = 'ask' | 'minimize' | 'close';

export interface AppSettings {
  start_with_windows: boolean;
  retention_days: number;
  max_item_count: number;
  hotkey_modifier: string;
  hotkey_key: string;
  // 连续粘贴的步进热键(每按一次从 FIFO 队列头出队一条并粘贴)
  seq_paste_modifier: string;
  seq_paste_key: string;
  paused: boolean;
  close_behavior: CloseBehavior;
  win_v_integration: boolean;
  pinned: boolean;
  // 「粘贴后保持打开」:单击条目后不隐藏面板(仅把焦点还给目标窗口)
  keep_open_on_paste: boolean;
  excluded_apps: string[];
  excluded_patterns: string[];
  detect_sensitive: boolean;
  // 误伤豁免:用户点过「仍要记录」的内容 hash
  excluded_allowlist: string[];
  // 短信验证码自动捕获(默认关闭,开启需授予系统通知访问权限)
  sms_code_enabled: boolean;
  // 云同步(WebDAV / S3)。口令与密钥不进这里,走 Windows 凭据管理器
  sync: SyncSettings;
}

export const DEFAULT_SETTINGS: AppSettings = {
  start_with_windows: false,
  retention_days: 30,
  max_item_count: 500,
  hotkey_modifier: 'Ctrl+Shift',
  hotkey_key: 'V',
  seq_paste_modifier: 'Ctrl',
  seq_paste_key: 'V',
  paused: false,
  close_behavior: 'ask',
  win_v_integration: false,
  pinned: true,
  keep_open_on_paste: false,
  excluded_apps: ['keepass.exe', 'keepassxc.exe', '1password.exe', 'bitwarden.exe'],
  excluded_patterns: [],
  detect_sensitive: true,
  excluded_allowlist: [],
  sms_code_enabled: false,
  sync: {
    enabled: false,
    provider: 'webdav',
    webdav_url: '',
    username: '',
    s3_endpoint: '',
    s3_region: '',
    s3_bucket: '',
    s3_access_key: '',
    s3_path_style: false,
    root_prefix: 'sparrow',
    poll_interval_sec: 5,
    device_name: '',
    sync_images: true,
    max_blob_mb: 20,
  },
};
