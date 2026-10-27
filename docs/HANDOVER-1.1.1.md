# 交接文档：剪雀 Sparrow 1.1.1（粘贴目标与连续粘贴修复）

> 版本编号说明：本文档已按新编号体系重写——更名前（Clipboard）为 0.x，更名后（Sparrow）从 1.0.0 起。文中 commit 哈希与日期为原始记录；§6 的产物体积/SHA-256 为 1.1.1 重建后的实测值。

面向接手 1.1.1 维护的人。读完应能独立完成：构建、真机验收、发版，并知道这一版为什么这样改、哪些是刻意保留的取舍。

**上游基线**：1.1.0（`42f957f`，`feat/cloud-sync` 合并后的 `master`）。1.1.0 的云同步架构、加密参数、同步语义见 `docs/HANDOVER-1.1.0.md`，本文不重复。

---

## 1. 版本与仓库

| 项目 | 值 |
| --- | --- |
| 版本 | **1.1.1** |
| 分支 | `master` |
| 修复提交 | `fd8ae39`（`fix: correct paste target handoff and continuous paste state`） |
| 发布提交 | `5a3fcb6`（`chore: release 2.1.1`，提交信息为原始记录，未按新编号改写） |
| 标签 | `v1.1.1`（附注标签） |
| GitHub Release | https://github.com/QikiiL/Sparrow/releases/tag/v1.1.1 |
| 安装包 | `sparrow-setup.exe` / `Sparrow_1.1.1_x64-setup.exe` |
| 便携版 | `sparrow.exe` |

1.1.1 只做缺陷修复，不改云同步协议与数据库结构；1.1.0 的同步数据、配置可直接沿用。

---

## 2. 这一版解决什么

用户报告三类问题，均在真实使用中复现：

1. **点击剪贴板条目，内容却粘进“上一个软件”**
   典型场景：浏览器网页输入框、微信聊天窗口。面板置顶且失焦不隐藏；用户先点目标输入框，再点面板条目，焦点却被强制还给唤出面板前的旧应用。
2. **开启「粘贴后保持打开」时，开始连续粘贴仍会隐藏面板**
   用户期望面板留在屏幕上，实际被销毁回后台。
3. **连续粘贴的计数与收尾状态不对**
   - 快速连按 Ctrl+V 时，个别按键被吞掉但不计数，队列少处理一条（例如 9 条只剩 1 条未粘）。
   - 连贴结束后再用热键唤出窗口，之前条目的绿色高亮/“已粘贴”样式残留，必须重新进一次连贴模式才消失。

---

## 3. 根因（按问题拆）

### 3.1 粘贴目标只在唤出时捕获一次

旧实现：

- `utils/hotkey.rs` 的 `show_with_paste_target()` 在**按热键唤出面板的瞬间**调用 `input_focus::capture_foreground_hwnd()`，写入 `PasteMode.target`；
- 此后点击条目，`services/paste_service.rs::release_focus_to_target()` **无条件**对这个值反复 `SetForegroundWindow`（最多 25×20ms）；
- 面板默认置顶（`default_pinned() -> true`），且没有 `WindowEvent::Focused(false)` 处理，所以“面板开着时切到另一个应用点输入框”是正常操作；
- 用户点条目时，旧 target 仍是唤出前的软件，焦点被强行拉回，Ctrl+V 自然落进旧窗口。

浏览器/微信之所以高发：它们经常有多个顶层窗口/独立聊天窗口，且用户习惯在面板与目标输入框之间来回点击；对代码而言，用户刚点的窗口与唤出时捕获的 hwnd 往往不是同一个。

### 3.2 keep-open 与连续粘贴没有打通

- `commands/continuous_paste.rs::start_continuous_paste()` 旧逻辑无条件调用 `release_focus_to_target()`，该函数内部 `destroy_main_window()`；
- `services/continuous_paste.rs::run_once_inner()` 只按“窗口是否可见”选择 `HideFirst`/`AtTarget`，从不读取 `keep_open_on_paste`；
- 结果：即使「粘贴后保持打开」开着，开始连贴仍然销毁面板；即使启动时不销毁，第一次粘贴也会走 `HideFirst` 再销毁。

### 3.3 快速连按丢步

- `utils/paste_hook.rs` 旧逻辑有 `DEBOUNCE_MS = 180`：180ms 内的第二次物理按键被**吞掉**，但不调用 `on_physical_trigger`；
- 用户快速连按 9 次时，其中一次可能落在 180ms 窗口内，队列只处理 8 次，剩 1 条；
- `services/continuous_paste.rs::run_once()` 旧结构在“最后一次 `take_pending_step()` 之后、`exit_delivery()` 之前”到达的触发会被记入 pending 但本轮不再消费，形成第二个丢步窗口。

### 3.4 连贴结束高亮残留

- `stores/continuousPasteStore.ts::setMode(false)` 旧实现清了 `selectedIds`/`selectionOrder`/`orderById`，但**没清 `status` 与 `pastedIds`**；
- `components/ClipboardItem.tsx` 的 `boxUsed = pastedIds.has(id)` 不判断 `mode`，而 `data-pasted` 会命中 `styles/globals.css` 的 `.clip-row[data-pasted]` 绿色样式；
- 因此连贴结束后，即使模式已经退出，行仍按“已粘贴”着色；徽标虽然不显示（徽标还有 `boxMode` 门禁），但整行绿色残留，看起来像选中态没清。

---

## 4. 修复设计

### 4.1 新增“最近外部前台窗口”跟踪器

文件：`src-tauri/src/utils/foreground_tracker.rs`（约 947 行，含单测）

- 启动：`lib.rs` 在 `app.manage(PasteMode::new())` 后调用 `foreground_tracker::start()`。
- 机制：专用线程安装 `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` + `GetMessageW` 消息泵；安装失败降级为 40ms 轮询。
- 记录三态（存储编码 `0 = Unknown`、`-1 = NoTarget`、`>0 = hwnd`）：

| 状态 | 含义 | `freeze_paste_target()` 结果 |
| --- | --- | --- |
| `Unknown` | 进程启动至今没有收到任何外部前台事件 | 回退到热键唤出时的 `captured`；`captured == 0` 时返回 `Some(0)`（托盘/冷启动旧路径） |
| `NoTarget` | 用户点了桌面/任务栏/开始菜单，或记录窗口已失效 | `None`：本次只复制，绝不激活任何窗口 |
| `Window(hwnd)` | 最近一个有效外部前台窗口 | `Some(hwnd)` |

- 过滤：
  - 必须 `IsWindow`、`IsWindowVisible`、非本进程、非 cloaked（`DwmGetWindowAttribute(DWMWA_CLOAKED)`，只在读取/校验时查，不在钩子回调里查）；
  - 排除任务栏/任务视图/开始菜单等壳窗口（类名见 `SHELL_CLASSES` / `DESKTOP_CLASSES`）；
  - 排除 `WS_EX_NOACTIVATE` 覆盖层；
  - 托盘溢出浮层（`NotifyIconOverflowWindow`、`TopLevelWindowForOverflowXamlIsland`）映射为 `Ignore`，保留下一条有效记录，避免“从溢出区打开面板 → 只剩只复制”；
  - `Windows.UI.Core.CoreWindow` 只有宿主进程为 `StartMenuExperienceHost` / `SearchHost` / `SearchApp` / `ShellExperienceHost` 时才当壳，避免误伤全屏 UWP 应用（照片/计算器）。
- 幂等/线程安全：`STARTED.swap` 防重入；钩子线程随进程存活，不卸载；记录用原子量，无锁。

### 4.2 点击时冻结落点，三态贯穿调用方

- `utils/input_focus.rs` 的 `PasteMode` 仍保留“热键唤出时捕获值”，但**不再**是唯一来源。
- `services/paste_service.rs::freeze_target_from_state()` → `foreground_tracker::freeze_paste_target(captured) -> Option<isize>`：
  - `Some(hwnd)`：明确落点；
  - `Some(0)`：无记录但允许 Z 序兜底/隐式激活（托盘/冷启动）；
  - `None`：只复制，**不激活任何窗口**。
- 冻结发生在 `deliver_item_by_id()` 第一行、任何 `await`/窗口操作之前；冻结值只按参数传递，**不写回** `PasteMode`（写回会把“唤出瞬间值”污染成上次粘贴的应用）。
- `KeepOpen` / `HideFirst` / `AtTarget` 三种焦点处理：

| 路径 | 何时使用 | 行为 |
| --- | --- | --- |
| `HideFirst` | 面板可见，且 keep-open 关闭 | 先销毁面板，再把前台交给冻结落点；交接失败 → 只复制 |
| `KeepOpen` | 面板可见，且 keep-open 开启 | 不销毁面板，只把焦点交给落点；落点无效 (`None`) → 只复制，不碰 Z 序；`Some(0)` 才 Z 序兜底；交接失败 → 降级 HideFirst，再失败才只复制 |
| `AtTarget` | 面板不可见（连贴的全局触发） | 不动焦点，粘贴到用户当前光标所在输入框 |

- 目标最小化时先 `ShowWindow(SW_RESTORE)`，否则 `SetForegroundWindow` 只会闪任务栏、Ctrl+V 落空。
- 仅复制模式（`force_paste=false`）：不冻结落点、不归还焦点；keep-open 关闭时仍销毁面板（保留“点一下有反应”的旧观感），keep-open 开启时面板保持打开、不碰焦点。

### 4.3 发送 Ctrl+V 前的前台闸门

`services/paste_service.rs::deliver_content()`：

- 写剪贴板（文本/链接/图片/文件/富文本均不变）；
- `input_focus::wait_for_paste_target(target)`：
  - `target != 0`：要求 `GetForegroundWindow() == target`，最多 15×20ms（约 300ms）；
  - `target == 0`：要求前台通过 `is_acceptable_paste_foreground`（拒绝任务栏/任务视图/NOACTIVATE/cloaked/本进程/瞬态浮层，**放行桌面**以保留“框选文件 → 连贴到桌面”），同样轮询约 300ms。
- 不匹配：**不发送 Ctrl+V**，剪贴板内容保留，返回 `Ok(false)`（`Delivery::Delivered { pasted: false }`）；日志 `[paste] foreground is not paste target ... — copy only`。
- 匹配：`simulate_paste()` 成功才返回 `pasted: true`；连续粘贴据此决定是否 `mark_pasted`。

### 4.4 连续粘贴

- `start_continuous_paste`：
  - 读取 `keep_open_on_paste`；
  - keep-open 且 `target.is_some()` → `paste_service::focus_target_keep_window(&app, target)`（内部 `FocusHandoffGuard`，不销毁窗口），成功后 `force_topmost` 保持置顶；
  - 否则（关闭/无落点/交接失败）→ `release_focus_to_target` 销毁兜底。
- `run_once_inner`：面板可见时按 `keep_open_on_paste` 选 `KeepOpen` / `HideFirst`；面板不可见时 `AtTarget`。
- `run_once`：
  - `try_enter_delivery` 防重入，失败则 `add_pending_step`；
  - 循环执行 `run_once_inner`；每轮成功后续期 busy、取 pending；
  - **最后一次 pending 检查之前** `exit_delivery`，`break` 后不再无条件 `exit_delivery`（避免清掉并发 `run_once` 新占用的 busy）；
  - `run_once_inner` 返回 `Err` 时才在错误分支 `exit_delivery`，条目已 `push_front` 留待重试。
- `Delivery::Delivered { pasted }`：只有真的发出 Ctrl+V 才 `mark_pasted`；`pasted=false` 只写剪贴板、出队但不打“已粘贴”标记（日志 `[continuous] item X not pasted — copied only`）。

### 4.5 触发键：keydown/keyup 配对

`utils/paste_hook.rs`：

- 删除 180ms 时间窗去抖；
- 新增 `TRIGGER_KEY_DOWN`：物理 keydown 且修饰键匹配时 `swap(true)`；keyup（`WM_KEYUP=0x0101` / `WM_SYSKEYUP=0x0105`）清 false；keyup 不吞，目标应用正常收键；
- 只有 `was_down == false` 才触发 `on_physical_trigger`；按住不放的自动重复被吞、不重复出队；
- 卡键自愈：`was_down == true` 但距上次触发超过 1500ms，视为新按下（keyup 丢失时避免钩子永久失效）；
- `install`/`uninstall` 复位标志；注入事件（`LLKHF_INJECTED`）不参与，`simulate_paste()` 自己的 Ctrl+V 不会被当成物理触发；
- 单测：`fire_on_first_press` / `ignore_auto_repeat_while_held` / `self_heal_stuck_key`。

### 4.6 前端连贴状态收尾

- `stores/continuousPasteStore.ts`：
  - `setMode(false)` 同时清 `status: null`、`pastedIds: EMPTY_SET`；
  - `refreshStatus`：`if (s.active) set({ mode: true })`；`else if (s.total > 0)` → `mode:false`、清 `status/pastedIds`，窗口重建时不恢复上一轮结束态。
- `components/ClipboardItem.tsx`：`boxUsed = s.mode && s.pastedIds.has(item.id)`，`data-pasted` 只在连贴模式内生效；`data-selected` 仍由 `boxSelected`（含 mode）控制。
- `ContinuousPasteBar` 的 3.5s 完成横幅逻辑保持不变：事件驱动，3.5s 后 `setMode(false)` 自动收尾。

---

## 5. 关键文件表

### Rust

| 文件 | 行数量级 | 职责 |
| --- | --- | --- |
| `src-tauri/src/utils/foreground_tracker.rs` | ~947 | 外部前台窗口跟踪、三态落点、窗口过滤、单测 |
| `src-tauri/src/services/paste_service.rs` | ~627 | 冻结落点、焦点交接、剪贴板写入、发送前闸门、`Delivery` |
| `src-tauri/src/services/continuous_paste.rs` | ~474 | 连贴 FIFO、busy/pending、`pasted` 标记、完成事件 |
| `src-tauri/src/utils/paste_hook.rs` | ~308 | 物理触发键低级钩子、keydown/keyup 配对、卡键自愈 |
| `src-tauri/src/utils/input_focus.rs` | ~238 | `SetForegroundWindow` 轮询、最小化恢复、Z 序兜底、前台可接受性 |
| `src-tauri/src/commands/continuous_paste.rs` | ~147 | 开始/自动/步进/取消连贴、keep-open 启动交接 |
| `src-tauri/src/commands/clipboard.rs` | — | `activate_item` 命令入口 |
| `src-tauri/src/lib.rs` | — | 启动 `foreground_tracker::start()` |

### 前端

| 文件 | 职责 |
| --- | --- |
| `src/stores/continuousPasteStore.ts` | 框选/连贴状态、`pastedIds`、状态同步与收尾清理 |
| `src/components/ClipboardItem.tsx` | 选中/已粘贴样式、`data-pasted` 门禁 |
| `src/components/ContinuousPasteBar.tsx` | 框选/执行/完成三阶段 UI，3.5s 自动退出 |

### 文档

| 文件 | 说明 |
| --- | --- |
| `docs/KEEP-OPEN-DESIGN.md` | keep-open 设计、焦点落点优先级、连贴 keep-open 行为、已知边界 |
| `docs/HANDOVER-1.1.1.md` | 本文件 |
| `docs/RELEASE.md` | 发版流程（版本号 6 处、打包、标签、Release） |

---

## 6. 构建与验证（1.1.1 实测）

| 项目 | 命令 | 结果 |
| --- | --- | --- |
| Rust 单测 | `cd src-tauri && cargo test --lib` | **269 passed / 0 failed / 4 ignored** |
| 类型检查 | `npx tsc --noEmit` | 0 error |
| 前端单测 | `npx vitest run` | **9 files / 66 tests passed** |
| 生产构建 | `npm run tauri build` | exit 0，NSIS 安装包生成成功 |
| Rust 检查 | `cargo check` | 0 warning / 0 error |
| Rust 格式 | `rustfmt --edition 2021 --check src-tauri/src/utils/foreground_tracker.rs` | exit 0 |

注意：

- `cargo test`（不带 `--lib`）会因 `build.rs` 给 bin target 链接 `requireAdministrator` manifest 而报 os error 740。**只跑 `--lib`**。
- 构建前如 `target/debug/sparrow.exe` 或 `target/release/sparrow.exe` 正在运行，链接期会报文件占用；先退出对应实例。`sms-helper.exe` / `sms-capture-helper.exe` 也要先停（见 `docs/RELEASE.md`）。
- `dist/` 的 `emptyOutDir` 为 `false`，打包前必须手动清空，否则旧前端文件会混进安装包。

### 发布产物

| 产物 | 大小 | SHA-256 |
| --- | --- | --- |
| `sparrow-setup.exe`（NSIS） | 7,186,292 B | `82945f18163781941bf4d49499711b802a73045bdbf77ba9cf2d896c679dbea3` |
| `sparrow.exe`（便携版） | 28,225,024 B | `5122ccb653a549cc42c040376dc93f8133646a579902c8c8683fbc1d02136673` |

> 注意：1.1.1 发布后补了一个 identity 版本常量修复（`EXPECTED_IDENTITY_VERSION` 0.2.1.4 → 0.2.1.5，与 AppxManifest 对齐），上表为**修复后**重新构建的产物。

GitHub Release 资产名固定为 `sparrow-setup.exe` / `sparrow.exe`；本地仓库根目录保留同版本副本（`.exe` 被 `.gitignore` 忽略）。

---

## 7. 发版记录

| 项目 | 值 |
| --- | --- |
| 修复提交 | `fd8ae39` |
| 版本提交 | `5a3fcb6`（`chore: release 2.1.1`，提交信息为原始记录，未按新编号改写） |
| 标签 | `v1.1.1` |
| Release | https://github.com/QikiiL/Sparrow/releases/tag/v1.1.1（非草稿/非预发布） |
| Release target | `master` |
| 已推送 | `master`、`v1.1.1` tag |
| 更新通道 | `version.json` 已为 1.1.1，`github` 指向 v1.1.1 |

发版顺序（本次实际执行，避免更新通道先于资产）：

1. 提交修复 + 版本号（6 个文件：`package.json`、`package-lock.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`、`version.json`）。
2. `npm run tauri build` 出安装包与便携版。
3. `git push origin v1.1.1`（只推 tag，不动 master）。
4. 用 `gh release create v1.1.1 ... sparrow-setup.exe sparrow.exe` 创建 Release 并上传资产。
5. 确认 Release 非草稿、资产大小正确后，`git push origin master`，让 `version.json` 更新通道生效。

> 蓝奏云是长期目录分享，不随版本改地址：<https://wwbwp.lanzoul.com/b00ybd8mej>（密码 `bdlx`）。**新版安装包需要手动上传**；本次发布只完成了 GitHub Release，蓝奏云未自动上传。

---

## 8. 已知限制与取舍（不要当 bug）

| 限制 | 影响 / 说明 |
| --- | --- |
| `NoTarget` → 只复制 | 用户点过桌面/任务栏，或目标窗口已关闭时，条目只写剪贴板、不激活任何窗口；这是刻意“宁少粘不粘错”。单击路径目前没有 UI toast，表现为“点了没粘贴”，需看 stderr 或手动 Ctrl+V。 |
| 长按触发键 > 1.5s 会再出队 | keyup 丢失的卡键自愈阈值。按住 Ctrl+V 不放超过 1.5s，可能每 1.5s 多粘一条；正常“点按”不受影响。 |
| 触发键配置切换竞态 | 按下与松开之间换触发键，keyup 可能不匹配；靠 1500ms 自愈或下一次 install/uninstall 复位。 |
| 壳窗口名单枚举 | 任务栏/开始菜单/任务视图/托盘溢出浮层都是类名/宿主进程枚举；Windows 未来改类名会退化为“只复制/可粘”，不会错粘。 |
| 全屏 UWP | `Windows.UI.Core.CoreWindow` 只在已知壳宿主进程下当壳；全屏 UWP 应用仍可作为落点，但需真机覆盖“照片/计算器”一类输入框。 |
| 托盘溢出区 | `NotifyIconOverflowWindow` / `TopLevelWindowForOverflowXamlIsland` 被 Ignore，落点回落到溢出前应用；若 Windows 再改类名，表现为“从溢出区打开后只复制”。 |
| keep-open 连贴中途降级 | 若每步 `refocus` 失败（目标被抢/窗口消失），`deliver_item_by_id` 仍会退回 `HideFirst` 销毁面板；这是保证内容粘出去的既有降级路径。 |
| HWND 回收 | 冻结到粘贴之间目标进程恰退出、句柄被复用，理论上可能粘错；概率极低，未做额外防护。 |
| 剪贴板 DB 仍未加密 | 1.1.0 的已知限制沿用：加密只发生在同步层，本机 `data/clipboard.db` 明文。 |
| 同步限制 | 1.1.0 的单周期无总超时、blob 串行、无断点续传、LWW 无逐条 UI 等限制不变。 |
| 仅复制模式无反馈 | 内容已进剪贴板，面板照常关闭（keep-open 时保留），但没有 toast；需要时在前端加轻量提示。 |

---

## 9. 排障入口

开发版（`npm run tauri dev`）stderr 里搜这些前缀：

| 日志 | 含义 |
| --- | --- |
| `[focus] paste target captured=... -> ... (source=tracker/tracker-stale/no-target/unknown->captured, age=...ms)` | 本次落点决策；`source=no-target`/`tracker-stale` 会退化为只复制 |
| `[focus] SetWinEventHook failed` / `polling fallback active (40ms)` | 钩子安装失败，已降级轮询；极端快速切换可能漏记一条 |
| `[paste] foreground is not paste target ... — copy only` | 发送前闸门拦下，剪贴板有内容但没发 Ctrl+V |
| `[keep-open] target=... self=... candidate=... focused=...` | keep-open 交接结果；`focused=false` 表示没交出去 |
| `[keep-open] no usable target — degrading to HideFirst path` | 落点无效，降级销毁面板 |
| `[continuous] item X not pasted — copied only` | 该条只复制，未标记“已粘贴”（计数会少一条，属如实反映） |

复现/验收重点：

1. keep-open 开 + 置顶开：浏览器点输入框 → 热键唤出 → 换另一个浏览器窗口点输入框 → 点条目；应粘进新窗口。
2. 微信两个独立聊天窗口重复。
3. keep-open 开：框选 9 条 → 开始连贴 → 面板应保留 → 快速按 9 次 Ctrl+V，9 条全部“已粘贴”。
4. 连贴结束后热键唤出：不应有绿色高亮残留。
5. keep-open 关：开始连贴应恢复旧行为（面板隐藏）。
6. 托盘溢出区打开面板 → 点条目；Win+Tab 后点条目；点桌面后点条目（应只复制，不拉回旧应用）；框选文件连贴到桌面。
7. 全屏 UWP 应用（照片/计算器）输入框。

---

## 10. 后续建议

1. **单击“只复制”提示**：`Delivery::Delivered { pasted: false }` 目前只有 stderr；建议 emit 一个轻量事件，状态栏提示“已复制（未找到可粘贴窗口）”。
2. **落点 TTL**：当前 `age` 只用于日志；如需“面板开太久不自动跳转”，可在 `freeze_paste_target` 加 5–10 分钟阈值，超时按 `NoTarget` 处理。
3. **连贴队列可视化**：后端 `QueueStatus` 目前只暴露 `next_id`/`pasted`，列表看不到完整队列；可加 `queued_ids`，并在连贴期间冻结列表排序，避免 `last_used_at` 更新导致行跳动、用户误判“少了一条”。
4. **触发键长按策略**：若用户反馈长按多粘，可改为只在“keyup 超时且无自动重复”时自愈，或提供设置项。
5. **壳窗口识别**：把类名/宿主进程白名单抽成配置或使用 UIA 的 `ControlType` 辅助判断，减少 Windows 版本升级带来的维护。
6. **E2E/真机自动化**：Win32 焦点行为无法用单测覆盖；可在 CI 外维护一台 Windows 真机，用 AutoHotkey/PowerShell 脚本做“浏览器/微信输入框 → 面板点击”的回归。

---

## 11. 下次发版检查清单

- [ ] 6 处版本号一致：`package.json`、`package-lock.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock`、`version.json`
- [ ] `version.json` 的 `version` / `notes` / `github` 已更新；中文用 UTF-8 写入，确认无 BOM
- [ ] 关闭正在运行的 `sparrow.exe` / `sms-helper.exe` / `sms-capture-helper.exe`
- [ ] 清空 `dist/` 并复核目录确实不存在
- [ ] `cargo test --lib`、`npx tsc --noEmit`、`npx vitest run`、`npm run tauri build` 全绿
- [ ] 安装包与便携版大小/哈希记录到本文件或 Release 说明
- [ ] 先推 tag，再建 GitHub Release 并上传两个资产，最后推 `master`（避免更新通道早于资产）
- [ ] 检查 Release 非草稿、非预发布，资产名固定为 `sparrow-setup.exe` / `sparrow.exe`
- [ ] 蓝奏云目录手动上传新安装包
- [ ] 更新 `docs/HANDOVER-<version>.md` 或追加到本文
