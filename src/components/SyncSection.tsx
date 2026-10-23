import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { AppSettings } from '../types/settings';
import type {
  SyncProvider,
  SyncSecretField,
  SyncSecretsPresence,
  SyncSettings,
} from '../types/sync';
import { useSyncStatus } from '../hooks/useSyncStatus';
import {
  Switch,
  Stepper,
  TextInput,
  Segmented,
  StatusDot,
  SectionLabel,
  Card,
  Row,
  ghostBtn,
} from './settings-controls';

/**
 * 「云同步」设置分区。
 *
 * 三条约束贯穿全文:
 * 1. 设置整对象覆盖 —— 任何一次保存都必须带 `sync`(走 patch() 合并);
 * 2. 口令 / 密码 / Secret Key 绝不进 settings,只走 `set_sync_secret`
 *    写进 Windows 凭据管理器;
 * 3. 文本类设置失焦才落盘,避免每敲一个字符就写一次 settings.json。
 */

const PROVIDERS: { value: SyncProvider; label: string }[] = [
  { value: 'webdav', label: 'WebDAV' },
  { value: 's3', label: 'S3 兼容' },
];

const MIN_POLL = 2;
const MAX_POLL = 300;

/** epoch ms → 中文相对时间(不引日期库) */
function formatRelative(ms: number): string {
  if (!ms) return '从未同步';
  const diff = Date.now() - ms;
  if (diff < 45_000) return '刚刚';
  const minutes = Math.floor(diff / 60_000);
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  return `${Math.floor(hours / 24)} 天前`;
}

/** 后端抛的错可能是 string(Rust Result 的 Err 就是字符串) */
function errText(err: unknown): string {
  if (typeof err === 'string') return err;
  if (err instanceof Error) return err.message;
  return String(err);
}

/**
 * 地址里内嵌 userinfo(`https://user:pass@host/...`)，或无 scheme 的
 * `user:pass@host` / `user@host` 形式(例如把 `AKIA:secret@s3.example.com`
 * 粘进 Access Key 或 Endpoint)。
 * 这种写法会把密码明文写进 settings.json,后端 WebDavBackend::new / S3Backend::new
 * 也会拒绝,所以前端先拦住:显示提示、且不调用 saveNow。
 */
const USERINFO_RE = /^\s*[a-z]+:\/\/[^/?#]*@/i;
/** 无 scheme 形式:第一个 `/`、空白或 `?`/`#` 之前就出现 `@` */
const BARE_USERINFO_RE = /^\s*[^/\s?#]+@/;

/**
 * 是否像"把凭证塞进了地址"。
 * `dav.example.com/path`、`https://dav.example.com` 都不命中;
 * `user@host`、`user:pass@host`、`https://user:pass@host` 命中。
 */
export function isUserinfoUrl(next: string): boolean {
  return USERINFO_RE.test(next) || BARE_USERINFO_RE.test(next);
}

/** WebDAV URL 与 S3 Endpoint 共用同一条校验(后端两个 new() 也是同一份检查) */
function validateSyncUrl(next: string): string | null {
  return isUserinfoUrl(next) ? '地址中不要包含用户名或密码' : null;
}

interface TextFieldProps {
  label: string;
  value: string;
  placeholder?: string;
  onCommit: (next: string) => void;
  /** 返回错误文案时:显示红色提示且不落盘(直到用户改掉) */
  validate?: (next: string) => string | null;
}

/** 带标签的文本设置项:本地草稿 + 失焦提交 */
function TextField({ label, value, placeholder, onCommit, validate }: TextFieldProps) {
  const [draft, setDraft] = useState(value);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    setDraft(value);
    setError(null);
  }, [value]);
  return (
    <label className="block">
      <span className="block mb-1 text-[11px] text-muted">{label}</span>
      <TextInput
        value={draft}
        onChange={(next) => {
          setDraft(next);
          // 改对了就立刻收回提示,不必等下一次失焦
          if (error && !validate?.(next)) setError(null);
        }}
        onBlur={() => {
          const invalid = validate?.(draft) ?? null;
          setError(invalid);
          if (invalid) return; // 有错不落盘,等用户改掉
          if (draft !== value) onCommit(draft);
        }}
        placeholder={placeholder}
        className="w-full"
      />
      {error && <p className="mt-1 text-danger text-[11px]">{error}</p>}
    </label>
  );
}

interface SecretFieldProps {
  label: string;
  field: SyncSecretField;
  saved: boolean;
  placeholder?: string;
  /** 提供按钮时改为显式点击保存(口令用它),否则失焦即保存 */
  withButton?: boolean;
  buttonLabel?: string;
  onSaved: () => void;
}

/** 凭证输入:只写凭据管理器,不进 settings */
function SecretField({
  label,
  field,
  saved,
  placeholder,
  withButton,
  buttonLabel = '保存',
  onSaved,
}: SecretFieldProps) {
  const [value, setValue] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const save = async () => {
    if (!value) return; // 空值不调用:避免把已存的凭证清空
    setBusy(true);
    setError(null);
    try {
      await invoke('set_sync_secret', { field, value });
      setValue('');
      onSaved();
    } catch (err) {
      setError(errText(err));
    } finally {
      setBusy(false);
    }
  };

  /** 空字符串 = 后端删除该条目(设置面板唯一能清掉已存凭证的入口) */
  const clear = async () => {
    setBusy(true);
    setError(null);
    try {
      await invoke('set_sync_secret', { field, value: '' });
      setValue('');
      onSaved();
    } catch (err) {
      setError(errText(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="block">
      <div className="flex items-center justify-between gap-2 mb-1">
        <span className="text-[11px] text-muted">{label}</span>
        {saved && (
          <span className="flex items-center gap-1.5 flex-shrink-0">
            <span className="text-[10.5px] text-ok">已保存</span>
            <button
              type="button"
              onClick={() => void clear()}
              disabled={busy}
              title="从 Windows 凭据管理器删除已保存的值"
              className="text-[10.5px] text-muted hover:text-danger underline decoration-dotted disabled:opacity-40"
            >
              清除
            </button>
          </span>
        )}
      </div>
      <div className="flex items-center gap-2">
        <TextInput
          value={value}
          onChange={setValue}
          type="password"
          placeholder={placeholder}
          onBlur={withButton ? undefined : () => void save()}
          className="flex-1 min-w-0"
        />
        {withButton && (
          <button
            type="button"
            onClick={() => void save()}
            disabled={busy || !value}
            className={`${ghostBtn} flex-shrink-0`}
          >
            {busy ? '保存中…' : buttonLabel}
          </button>
        )}
      </div>
      {error && <p className="mt-1 text-[10.5px] text-danger">{error}</p>}
    </div>
  );
}

interface Props {
  settings: AppSettings;
  saveNow: (next: AppSettings) => void;
}

export function SyncSection({ settings, saveNow }: Props) {
  const s = settings.sync;
  const status = useSyncStatus();

  /** 整对象覆盖:必须合并到 settings.sync 上再保存 */
  const patch = (partial: Partial<SyncSettings>) => {
    // 双保险:内嵌 userinfo 的地址一律不落盘(单字段校验在 TextField 里)。
    // WebDAV 与 S3 两条地址都拦:后端两个 new() 都会拒绝这种写法。
    if (partial.webdav_url !== undefined && validateSyncUrl(partial.webdav_url)) return;
    if (partial.s3_endpoint !== undefined && validateSyncUrl(partial.s3_endpoint)) return;
    saveNow({ ...settings, sync: { ...s, ...partial } });
  };

  // 已保存的凭证(只回显布尔,不读回明文)
  const [secrets, setSecrets] = useState<SyncSecretsPresence | null>(null);
  const refreshSecrets = useCallback(() => {
    invoke<SyncSecretsPresence>('has_sync_secrets')
      .then(setSecrets)
      .catch(() => {});
  }, []);
  useEffect(() => {
    refreshSecrets();
  }, [refreshSecrets]);

  // 轮询间隔:本地草稿 + 失焦/步进落盘;用 ref 避开 setState 异步导致读到旧值
  const [poll, setPoll] = useState(s.poll_interval_sec || 5);
  const pollRef = useRef(poll);
  useEffect(() => {
    const v = s.poll_interval_sec || 5;
    pollRef.current = v;
    setPoll(v);
  }, [s.poll_interval_sec]);
  const setPollDraft = (n: number) => {
    const v = Math.min(MAX_POLL, Math.max(MIN_POLL, Number.isFinite(n) ? Math.trunc(n) : MIN_POLL));
    pollRef.current = v;
    setPoll(v);
  };
  const commitPoll = () => {
    if (pollRef.current !== s.poll_interval_sec) patch({ poll_interval_sec: pollRef.current });
  };

  const [testing, setTesting] = useState(false);
  const [syncing, setSyncing] = useState(false);
  const [actionResult, setActionResult] = useState<{ ok: boolean; text: string } | null>(null);

  const handleTest = async () => {
    setTesting(true);
    setActionResult(null);
    try {
      const msg = await invoke<string>('test_sync_connection');
      setActionResult({ ok: true, text: msg || '连接正常' });
    } catch (err) {
      // 失败只显示后端给的 message,不自行猜测原因
      setActionResult({ ok: false, text: errText(err) });
    } finally {
      setTesting(false);
    }
  };

  const handleSyncNow = async () => {
    setSyncing(true);
    setActionResult(null);
    try {
      await invoke('sync_now');
      setActionResult({ ok: true, text: '已开始同步' });
    } catch (err) {
      setActionResult({ ok: false, text: errText(err) });
    } finally {
      setSyncing(false);
    }
  };

  const state = status?.state ?? 'disabled';
  const tone = state === 'error' ? 'danger' : state === 'syncing' ? 'warn' : state === 'idle' ? 'ok' : 'idle';
  const statusText =
    state === 'syncing'
      ? '同步中…'
      : state === 'error'
        ? `错误：${status?.last_error || '未知错误'}`
        : state === 'idle'
          ? status?.last_sync_at
            ? `已同步 · ${formatRelative(status.last_sync_at)}`
            : '已同步'
          : '未启用';

  return (
    <>
      <SectionLabel>云同步</SectionLabel>
      <Card>
        <div className="px-3.5 py-[11px]">
          <div className="flex items-start justify-between gap-3">
            <div className="min-w-0">
              <div className="text-[12.5px] font-medium">云同步</div>
              <p className="mt-[3px] text-[11px] leading-relaxed text-faint">
                用你自己的 WebDAV / S3 存储在多台设备间同步；内容在本地加密后上传
              </p>
            </div>
            <div
              className="flex items-center gap-1.5 flex-shrink-0 text-[11px] text-muted"
              title={state === 'error' ? (status?.last_error ?? '同步失败') : statusText}
            >
              <StatusDot tone={tone} />
              <span className="max-w-[180px] truncate">{statusText}</span>
            </div>
          </div>
        </div>

        <Row title="启用云同步" desc="关闭后不再上传与拉取，本地历史不受影响">
          <Switch
            checked={s.enabled}
            onChange={() => patch({ enabled: !s.enabled })}
            label="启用云同步"
          />
        </Row>

        <div className="px-3.5 py-[11px]">
          <div className="text-[12.5px] font-medium">存储后端</div>
          <div className="mt-2">
            <Segmented
              value={s.provider}
              options={PROVIDERS}
              onChange={(v) => patch({ provider: v })}
            />
          </div>
        </div>

        {s.provider === 'webdav' ? (
          <div className="px-3.5 py-[11px] space-y-2.5">
            <TextField
              label="服务器 URL"
              value={s.webdav_url}
              placeholder="https://dav.example.com/remote.php/dav/files/you/sparrow"
              validate={validateSyncUrl}
              onCommit={(v) => patch({ webdav_url: v })}
            />
            <TextField
              label="用户名"
              value={s.username}
              placeholder="alice"
              onCommit={(v) => patch({ username: v })}
            />
            <SecretField
              label="密码"
              field="webdav_password"
              saved={!!secrets?.webdav_password}
              placeholder="已保存在 Windows 凭据管理器"
              onSaved={refreshSecrets}
            />
          </div>
        ) : (
          <div className="px-3.5 py-[11px] space-y-2.5">
            <TextField
              label="Endpoint"
              value={s.s3_endpoint}
              placeholder="https://s3.us-east-1.amazonaws.com"
              validate={validateSyncUrl}
              onCommit={(v) => patch({ s3_endpoint: v })}
            />
            <TextField
              label="Region"
              value={s.s3_region}
              placeholder="us-east-1"
              onCommit={(v) => patch({ s3_region: v })}
            />
            <TextField
              label="Bucket"
              value={s.s3_bucket}
              placeholder="sparrow-sync"
              onCommit={(v) => patch({ s3_bucket: v })}
            />
            <TextField
              label="Access Key"
              value={s.s3_access_key}
              placeholder="AKIA…"
              onCommit={(v) => patch({ s3_access_key: v })}
            />
            <SecretField
              label="Secret Key"
              field="s3_secret_key"
              saved={!!secrets?.s3_secret_key}
              placeholder="已保存在 Windows 凭据管理器"
              onSaved={refreshSecrets}
            />
            <div className="flex items-center justify-between gap-3">
              <div className="min-w-0">
                <div className="text-[12.5px] font-medium">路径风格寻址</div>
                <p className="mt-[2px] text-[11px] text-faint">自建 MinIO 等通常需要开启</p>
              </div>
              <Switch
                checked={s.s3_path_style}
                onChange={() => patch({ s3_path_style: !s.s3_path_style })}
                label="路径风格寻址"
              />
            </div>
          </div>
        )}

        <Row title="轮询间隔" desc={`${MIN_POLL}–${MAX_POLL} 秒；越小越实时，也越费流量`}>
          <Stepper
            value={poll}
            min={MIN_POLL}
            ariaLabel="轮询间隔"
            onChange={setPollDraft}
            onCommit={commitPoll}
          />
        </Row>

        <div className="px-3.5 py-[11px] space-y-2.5">
          <TextField
            label="设备名"
            value={s.device_name}
            placeholder="用来在其它设备上认出这条快照"
            onCommit={(v) => patch({ device_name: v })}
          />
          <TextField
            label="根前缀"
            value={s.root_prefix}
            placeholder="sparrow"
            onCommit={(v) => patch({ root_prefix: v })}
          />
        </div>

        <Row title="同步图片" desc="图片单独加密上传，会占用更多流量">
          <Switch
            checked={s.sync_images}
            onChange={() => patch({ sync_images: !s.sync_images })}
            label="同步图片"
          />
        </Row>

        <div className="px-3.5 py-[11px]">
          <div className="text-[12.5px] font-medium">同步口令</div>
          <div className="mt-2">
            <SecretField
              label="口令"
              field="passphrase"
              saved={!!secrets?.passphrase}
              placeholder="所有设备使用同一口令"
              withButton
              buttonLabel="保存口令"
              onSaved={refreshSecrets}
            />
          </div>
          <p className="mt-1.5 text-[11px] leading-relaxed text-faint">
            所有设备需输入同一口令；口令丢失后远端数据无法恢复
          </p>
        </div>

        <div className="px-3.5 py-[11px]">
          <div className="flex items-center gap-2 flex-wrap">
            <button onClick={() => void handleTest()} disabled={testing} className={ghostBtn}>
              {testing ? '测试中…' : '测试连接'}
            </button>
            <button onClick={() => void handleSyncNow()} disabled={syncing} className={ghostBtn}>
              {syncing ? '同步中…' : '立即同步'}
            </button>
            {actionResult && (
              <span
                className={`text-[11px] min-w-0 truncate ${actionResult.ok ? 'text-ok' : 'text-danger'}`}
                title={actionResult.text}
              >
                {actionResult.text}
              </span>
            )}
          </div>
          <p className="mt-2 text-[10.5px] font-mono text-faint truncate" title="本机设备 ID">
            设备 ID：{status?.device_id || '首次同步后生成'}
          </p>
        </div>
      </Card>
    </>
  );
}
