import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { SyncSection, isUserinfoUrl } from '../SyncSection';
import { DEFAULT_SETTINGS } from '../../types/settings';
import type { AppSettings } from '../../types/settings';

// 两个 mock 必须走 vi.hoisted:vi.mock 的工厂函数被提升到文件顶部,
// 直接引用普通 const 会踩 TDZ。
const { invokeMock, listenMock, unlistenMock } = vi.hoisted(() => {
  const unlisten = vi.fn();
  return { invokeMock: vi.fn(), listenMock: vi.fn(), unlistenMock: unlisten };
});

vi.mock('@tauri-apps/api/core', () => ({ invoke: invokeMock }));
vi.mock('@tauri-apps/api/event', () => ({ listen: listenMock }));

function settings(overrides: Partial<AppSettings['sync']> = {}): AppSettings {
  return {
    ...DEFAULT_SETTINGS,
    sync: { ...DEFAULT_SETTINGS.sync, enabled: true, ...overrides },
  };
}

/** 只关心命令名与参数,统一返回可解析的桩值 */
function stubInvoke() {
  invokeMock.mockImplementation((cmd: string) => {
    switch (cmd) {
      case 'has_sync_secrets':
        return Promise.resolve({
          webdav_password: false,
          s3_secret_key: false,
          passphrase: false,
        });
      case 'get_sync_status':
        return Promise.resolve({
          state: 'idle',
          device_id: '',
          last_sync_at: 0,
          last_error: null,
          pending: 0,
        });
      case 'test_sync_connection':
        return Promise.resolve('连接正常');
      default:
        return Promise.resolve(null);
    }
  });
}

const calledCommands = () => invokeMock.mock.calls.map((c) => c[0] as string);

describe('SyncSection · 云同步设置分区', () => {
  beforeEach(() => {
    invokeMock.mockReset();
    listenMock.mockReset();
    listenMock.mockResolvedValue(unlistenMock);
    unlistenMock.mockReset();
    stubInvoke();
  });

  it('渲染出分区标题、启用开关与操作按钮', async () => {
    render(<SyncSection settings={settings()} saveNow={() => {}} />);

    // 分区标签 + 卡片标题各一个
    expect(screen.getAllByText('云同步').length).toBeGreaterThan(0);
    expect(screen.getByText('启用云同步')).toBeTruthy();
    expect(screen.getByRole('button', { name: '测试连接' })).toBeTruthy();
    expect(screen.getByRole('button', { name: '立即同步' })).toBeTruthy();
    // 状态行:初值 idle → 显示「已同步」
    await waitFor(() => expect(screen.getByText(/已同步/)).toBeTruthy());
  });

  it('点「测试连接」调用 test_sync_connection 并回显结果', async () => {
    render(<SyncSection settings={settings()} saveNow={() => {}} />);

    fireEvent.click(screen.getByRole('button', { name: '测试连接' }));

    await waitFor(() => expect(calledCommands()).toContain('test_sync_connection'));
    await waitFor(() => expect(screen.getByText('连接正常')).toBeTruthy());
  });

  it('WebDAV 密码只走 set_sync_secret,不进 save_settings', async () => {
    const saveNow = vi.fn();
    render(<SyncSection settings={settings()} saveNow={saveNow} />);

    const password = screen.getByPlaceholderText('已保存在 Windows 凭据管理器');
    fireEvent.change(password, { target: { value: 'p@ssw0rd-zz' } });
    fireEvent.blur(password);

    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('set_sync_secret', {
        field: 'webdav_password',
        value: 'p@ssw0rd-zz',
      })
    );

    // 同时改一个普通字段,确保真的产生了一次 save_settings 的 payload
    const url = screen.getByPlaceholderText(
      'https://dav.example.com/remote.php/dav/files/you/sparrow'
    );
    fireEvent.change(url, { target: { value: 'https://dav.example.com/sparrow' } });
    fireEvent.blur(url);
    await waitFor(() => expect(saveNow).toHaveBeenCalled());

    // 关键断言:整对象覆盖的保存路径里绝不能出现密码或密码字段名
    const savedPayloads = JSON.stringify(saveNow.mock.calls.map((c) => (c[0] as AppSettings).sync));
    expect(savedPayloads).toContain('https://dav.example.com/sparrow');
    expect(savedPayloads).not.toContain('webdav_password');
    expect(savedPayloads).not.toContain('p@ssw0rd-zz');
  });

  it('WebDAV URL 内嵌 userinfo 时显示提示且不落盘', async () => {
    const saveNow = vi.fn();
    render(<SyncSection settings={settings()} saveNow={saveNow} />);

    const url = screen.getByPlaceholderText(
      'https://dav.example.com/remote.php/dav/files/you/sparrow'
    );
    fireEvent.change(url, { target: { value: 'https://u:p@dav.example.com/sparrow' } });
    fireEvent.blur(url);

    // 提示文案来自 TextField 的 validate,且失焦不提交
    await waitFor(() => expect(screen.getByText('地址中不要包含用户名或密码')).toBeTruthy());
    expect(saveNow).not.toHaveBeenCalled();
    expect(calledCommands()).not.toContain('save_settings');
  });

  it('S3 Endpoint 内嵌 userinfo 同样被拦下(复用同一条校验)', async () => {
    const saveNow = vi.fn();
    render(<SyncSection settings={settings({ provider: 's3' })} saveNow={saveNow} />);

    const endpoint = screen.getByPlaceholderText('https://s3.us-east-1.amazonaws.com');
    fireEvent.change(endpoint, { target: { value: 'https://AKIA:secret@s3.example.com' } });
    fireEvent.blur(endpoint);

    await waitFor(() => expect(screen.getByText('地址中不要包含用户名或密码')).toBeTruthy());
    expect(saveNow).not.toHaveBeenCalled();
  });

  it('S3 Endpoint 输入无 scheme 的 AKIA:secret@host 同样被拦下', async () => {
    const saveNow = vi.fn();
    render(<SyncSection settings={settings({ provider: 's3' })} saveNow={saveNow} />);

    const endpoint = screen.getByPlaceholderText('https://s3.us-east-1.amazonaws.com');
    fireEvent.change(endpoint, { target: { value: 'AKIA:secret@s3.example.com' } });
    fireEvent.blur(endpoint);

    await waitFor(() => expect(screen.getByText('地址中不要包含用户名或密码')).toBeTruthy());
    expect(saveNow).not.toHaveBeenCalled();
    expect(calledCommands()).not.toContain('save_settings');
  });

  it('isUserinfoUrl 命中带/不带 scheme 的 userinfo,放过普通地址', () => {
    // 命中:把凭证塞进地址的三种写法
    expect(isUserinfoUrl('user@host')).toBe(true);
    expect(isUserinfoUrl('user:pass@host')).toBe(true);
    expect(isUserinfoUrl('https://user:pass@dav.example.com/x')).toBe(true);
    // 不命中:没有 `@` 的正常地址(含无 scheme 与带 scheme)
    expect(isUserinfoUrl('dav.example.com/path')).toBe(false);
    expect(isUserinfoUrl('https://dav.example.com')).toBe(false);
  });

  it('点「清除」用空串调 set_sync_secret 删掉已存凭证', async () => {
    // 只有 has_sync_secrets 说"已保存"时才会渲染「清除」
    invokeMock.mockImplementation((cmd: string) => {
      switch (cmd) {
        case 'has_sync_secrets':
          return Promise.resolve({
            webdav_password: true,
            s3_secret_key: false,
            passphrase: false,
          });
        case 'get_sync_status':
          return Promise.resolve({
            state: 'idle',
            device_id: '',
            last_sync_at: 0,
            last_error: null,
            pending: 0,
          });
        default:
          return Promise.resolve(null);
      }
    });

    render(<SyncSection settings={settings()} saveNow={() => {}} />);

    const clear = await screen.findByRole('button', { name: '清除' });
    fireEvent.click(clear);

    // 空字符串是后端"删除该条目"的唯一信号
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith('set_sync_secret', {
        field: 'webdav_password',
        value: '',
      })
    );
  });

  it('切换「仅启动时」保存 sync_mode 并隐藏轮询间隔', async () => {
    const saveNow = vi.fn();
    const { rerender } = render(<SyncSection settings={settings()} saveNow={saveNow} />);

    // 默认是轮询模式:间隔 Stepper 在
    expect(screen.getByText('轮询间隔')).toBeTruthy();

    fireEvent.click(screen.getByRole('button', { name: '仅启动时' }));

    await waitFor(() => expect(saveNow).toHaveBeenCalled());
    const calls = saveNow.mock.calls;
    const payload = calls[calls.length - 1][0] as AppSettings;
    expect(payload.sync.sync_mode).toBe('startup');
    // 整对象覆盖:其它字段不能被这次切换抹掉
    expect(payload.sync.enabled).toBe(true);

    // 父组件把新设置回传后:轮询间隔消失,改为一行说明
    rerender(<SyncSection settings={settings({ sync_mode: 'startup' })} saveNow={saveNow} />);
    await waitFor(() => expect(screen.queryByText('轮询间隔')).toBeNull());
    expect(screen.getByText(/启动时同步一次，之后不轮询/)).toBeTruthy();
    // 状态行与手动同步按钮在两种模式下都在
    expect(screen.getByRole('button', { name: '立即同步' })).toBeTruthy();
    await waitFor(() => expect(screen.getByText(/已同步/)).toBeTruthy());
  });

  it('显示后端上报的跳过条目告警', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'has_sync_secrets') {
        return Promise.resolve({ webdav_password: false, s3_secret_key: false, passphrase: false });
      }
      if (cmd === 'get_sync_status') {
        return Promise.resolve({
          state: 'idle',
          device_id: '',
          last_sync_at: Date.now(),
          last_error: null,
          pending: 0,
          warning: '3 张图片未同步',
          skipped: 3,
        });
      }
      return Promise.resolve(null);
    });

    render(<SyncSection settings={settings()} saveNow={() => {}} />);

    await waitFor(() => expect(screen.getByText(/3 张图片未同步/)).toBeTruthy());
  });

  it('同步中显示后端上报的进度点(phase)', async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === 'has_sync_secrets') {
        return Promise.resolve({ webdav_password: false, s3_secret_key: false, passphrase: false });
      }
      if (cmd === 'get_sync_status') {
        return Promise.resolve({
          state: 'syncing',
          device_id: '',
          last_sync_at: 0,
          last_error: null,
          pending: 0,
          phase: '同步条目 3/20',
        });
      }
      return Promise.resolve(null);
    });

    render(<SyncSection settings={settings()} saveNow={() => {}} />);

    // 进度点替换掉笼统的「同步中…」:用户能看到首次全量同步走到哪一步
    await waitFor(() => expect(screen.getByText('同步中 · 同步条目 3/20')).toBeTruthy());
    expect(screen.queryByText('同步中…')).toBeNull();
  });
});
