# 前端交接结论 · clipboard-manager-tauri

> 交接时点：master `a99b035`，版本 **v0.6.1**（tag `v0.6.1`，2026-09-28 打包）。
> 前端健康度：`npx tsc --noEmit` 零错误；`npx vitest run` 20/20 通过；`npm run build` 可构建。
> 本文只覆盖**前端**（`src/`，34 个 ts/tsx，约 4490 行）及它与后端的契约。后端不在范围内，
> 只在"契约"和"不变量"两节列出前端必须知道的部分。

---

## 0. 一句话定位

一个 Windows 剪贴板历史面板，Tauri 2 + React 19 + TS + Tailwind v4 + Zustand。
**它不是常驻窗口的普通 SPA**：主窗口"隐藏即销毁、唤出即重建"，所以每次唤出都是一次
**全新挂载 + 重新查询**，前端状态天然不存活。这一条决定了前端几乎所有设计取舍。

---

## 1. 技术栈与规模

| 项 | 值 |
|---|---|
| 框架 | React 19（StrictMode）+ TypeScript 5.8 |
| 构建 | Vite 7（`emptyOutDir: false`，见 §8）+ `@vitejs/plugin-react` |
| 样式 | Tailwind CSS v4（`@tailwindcss/vite`，**无 tailwind.config 主题扩展**，全部走 CSS 变量） |
| 状态 | Zustand v5（两个 store，无持久化中间件） |
| 列表 | `@tanstack/react-virtual` 虚拟滚动 |
| 与后端 | `@tauri-apps/api`（core/event/window/app）+ `@tauri-apps/plugin-sql`（**只读**） |
| 测试 | Vitest 4 + jsdom + @testing-library/react（仅 renderHook 用到） |

设计基准尺寸 420×520 逻辑像素（后端 `window_sizing.rs` 的 `DESIGN_W/H`）。

---

## 2. 目录地图

```
src/
├─ main.tsx                 12   挂载 + StrictMode + globals.css
├─ App.tsx                  267  唯一页面：标题栏(置顶/保持打开/连贴/设置/最小化/最大化/关闭)
│                                 + 更新检查节流 + 关闭确认 + 各弹窗宿主
├─ contexts/
│  └─ ThemeContext.tsx       44  light/dark，localStorage 'theme' + html.dark 类
├─ stores/
│  ├─ clipboardStore.ts      55  列表/分组/搜索/收藏过滤/加载态/暂停/查询上限/requestId
│  └─ continuousPasteStore.ts 262 连续粘贴：框选集合+点选顺序+后端队列状态镜像+全部动作
├─ hooks/
│  ├─ useDatabase.ts         82  loadItems（带 requestId 竞态防护）/ loadGroups / 启动同步设置
│  ├─ useClipboardListener.ts 56 事件订阅 + store 订阅 → 触发重查
│  ├─ useRubberBandSelect.ts 181 框选橡皮筋（累积制 + wheel 滚动累积 + didDragRef）
│  └─ useDebounce.ts         12
├─ lib/
│  ├─ queryBuilder.ts        40  SQL 拼装 + escapeLike（必须配对 ESCAPE）
│  ├─ db.ts                  27  plugin-sql 单例；queryItems 只读入口
│  ├─ continuousPaste.ts     30  纯函数：点选顺序 → 粘贴队列 → 序号 map
│  ├─ format.ts              19  dbTimeToDate（UTC 修正）/ formatBytes
│  └─ openUrl.ts              6  走后端白名单 open_external_url
├─ types/
│  ├─ clipboard.ts           19  ClipboardType 枚举(0-3) + ClipboardItem
│  ├─ settings.ts            45  AppSettings + DEFAULT_SETTINGS
│  └─ group.ts                7
├─ components/
│  ├─ SettingsPanel.tsx      947 最大文件：全部设置项，即改即存（6 分区卡片式布局）
│  ├─ settings-controls.tsx  175 设置面板可复用控件：Switch/Stepper/Keycaps/SectionLabel/Card/Row + 幽灵按钮类名
│  ├─ ClipboardItem.tsx      462 单行卡片：类型图标/图片缩略图+悬停大图/时间/操作按钮/分组菜单/框选徽标
│  ├─ StatusBar.tsx          259 统计 + 单击粘贴/复制切换 + 监听状态 + 两类居中提示
│  ├─ icons.tsx              254 内联 SVG（无图标库）
│  ├─ ContinuousPasteBar.tsx 224 连续粘贴三段式横幅（框选/执行/完成）
│  ├─ ContentEditorDialog.tsx 180 查看/编辑条目内容（仅非图片）
│  ├─ GroupTabs.tsx          155 分组标签 + 右键菜单 + 新建/重命名/删除
│  ├─ Dialogs.tsx            154 PromptDialog / ConfirmDialog（替代原生 prompt/confirm）
│  ├─ ClipboardList.tsx      154 虚拟列表 + 框选接线 + 空/加载态
│  ├─ UpdateDialog.tsx       120 更新弹窗（GitHub/蓝奏云双通道 + 密码复制）
│  ├─ CloseConfirmDialog.tsx  78
│  └─ SearchBar.tsx           58 300ms 防抖 + Ctrl+F 聚焦 + Esc 清空
└─ tests/                    179 4 个文件，20 个用例（见 §8）
```

---

## 3. 数据流

### 3.1 列表重查有且只有一个入口

```
后端事件(clipboard-changed / item-deleted)
store 变更(searchQuery / selectedGroup / showFavorites / maxItems)
        │
        ▼
useClipboardListener → loadItems(useDatabase) → buildItemQuery → queryItems(plugin-sql)
        │
        ▼
setItems（仅在 requestId 仍是最新时才落地）
```

**不要在组件里再调 `loadItems`**。`GroupTabs` 顶部注释已写明：事件的订阅已经覆盖，组件里
补一次会导致一次操作发 2–3 遍相同查询。改列表过滤逻辑时，先确认是不是该动
`useClipboardListener` 的订阅条件，而不是在按钮上补调用。

### 3.2 写操作一律走 Rust 命令，前端 SQL 只做 SELECT

`lib/db.ts` 的注释是硬约定：**只读**。删除/收藏/分组/编辑内容/清空历史全部 `invoke`。
plugin-sql 与后端 sqlx 是**双连接**（WAL + busy_timeout=5000），前端写会引发锁竞争。

### 3.3 状态分层（重要）

| 状态 | 放哪 | 原因 |
|---|---|---|
| 连续粘贴队列 | **后端**（`ContinuousPasteState`） | 窗口销毁重建不能丢进度；前端只是事件快照的镜像 |
| 置顶 / 保持打开 / 单击粘贴模式 | 后端（设置 + `set_always_on_top` 等命令） | 跨窗口存活 |
| 主题 | localStorage `theme` | 跨窗口存活 |
| 更新检查时间戳 / 已忽略版本 | localStorage `clipboard-update-checked-at` / `-dismissed` | 30 分钟节流要跨窗口 |
| 列表/分组/搜索/框选 | Zustand 内存 | 每次唤出重查重建，丢失无妨 |

**新增需要"跨窗口存活"的状态，先问一句：能不能放后端？不能才用 localStorage。**

---

## 4. 前后端契约

### 4.1 命令（前端 invoke ⇄ 后端 generate_handler）

已核对：**前端调用的 41 个命令全部在后端注册，无悬空调用**。
（`npx` 级别的核对方法见 §8。）

按模块：

| 模块 | 命令 |
|---|---|
| 条目 | `activate_item` `delete_item` `toggle_favorite` `update_item_content` `clear_history` `pause_monitoring` |
| 连续粘贴 | `start_continuous_paste` `start_continuous_paste_auto` `continuous_paste_next` `stop_continuous_paste_auto` `get_continuous_paste_status` `cancel_continuous_paste` |
| 设置/存储 | `load_settings` `save_settings` `allow_excluded_item` `set_keep_open` `get_storage_info` `change_storage_location` `reset_storage_location` |
| 窗口 | `show_window` `hide_window` `close_app` `register_hotkey` `get_close_behavior` `set_close_behavior` `enable_win_v_integration` `disable_win_v_integration` `set_always_on_top` `get_paste_mode` `set_paste_mode` |
| 分组 | `create_group` `update_group` `delete_group` `set_item_group` |
| 其它 | `get_db_path` `check_update` `open_external_url` `write_clipboard_text` `sms_code_status` `sms_code_request_access` `open_notification_settings` |

参数命名：前端写 camelCase（`keepOpen` / `intervalMs` / `itemId`），Rust 侧 snake_case，
Tauri 2 自动转换。**新增命令时两端都要按各自风格写，不要手动对齐成同一套。**

后端注册但前端未调用（3 个，死代码候选，但**不要顺手删**）：
`show_window`、`get_close_behavior`、`get_image_base64`。
前两个是窗口链路的对称 API，`get_image_base64` 是图片走 asset 协议之前的旧路径。

### 4.2 事件（后端 emit → 前端 listen）

| 事件 | 载荷 | 前端消费 |
|---|---|---|
| `clipboard-changed` | — | 重查列表 |
| `item-deleted` | — | 重查列表 |
| `groups-changed` | — | 重查分组；选中分组若已删除则置 null |
| `monitoring-paused` | `boolean` | 状态栏圆点 |
| `paste-mode-changed` | `boolean` | 状态栏「单击粘贴/单击复制」 |
| `item-excluded` | `{reason, hash}` | 居中提示 + 「仍要记录」→ `allow_excluded_item` |
| `sms-code-copied` | `{code, sender}` | 居中提示（与排除提示共容器，自动堆叠） |
| `ask-close-behavior` | — | 关闭确认弹窗 |
| `update-available` | `UpdateInfo` | 更新弹窗（尊重「本版本已忽略」） |
| `continuous-paste` | `{event, status?, message?}` | 连续粘贴横幅全部状态；`event` ∈ updated/pasted/finished/error/cancelled |

### 4.3 三处必须同步的类型（改一处会静默出错）

1. **`types/settings.ts`**：`AppSettings` 接口 **+** `DEFAULT_SETTINGS`
   ⇄ `models/settings.rs` 的 struct **+** `impl Default` / 各 `#[serde(default = ...)]`
   > 后端对新字段**必须**带 `#[serde(default = "default_xxx")]`（bool 尤其不能裸写
   > `#[serde(default]`，那会默认 false，等于给老用户静默关掉特性）。
   > 前端 `SettingsPanel` 打开时会 `{ ...DEFAULT_SETTINGS, ...s }` 合并，靠这层兜底。
2. **`types/clipboard.ts` `ClipboardType`** 0/1/2/3 ⇄ `models/clipboard_type.rs` `#[repr(i32)]`
3. **`stores/continuousPasteStore.ts` `ContinuousStatus`** ⇄ `services/continuous_paste.rs QueueStatus`

---

## 5. 改前端前必须知道的架构不变量

1. **隐藏即销毁 / 唤出即重建**。`tauri.conf.json` 里 `windows: []`，窗口由后端
   `create_main_window` 唯一路径创建。前端不要试图"保持"任何跨唤出的内存状态。
2. **禁止绕过 Tauri 直接调 WebView2 API**（`controller.Close()` / `TrySuspend` / 手动
   `SetIsVisible`）——历史上造成过白屏与输入失效。窗口控制一律走 `getCurrentWindow()`。
3. **图片经 asset 协议加载**（`convertFileSrc(file_path)`），不经 IPC/base64。
   `file_path` 来自数据库属**不可信输入**：删除/粘贴由后端 `resolve_image_path` 守卫，
   前端只负责展示；**新增任何图片打开点都要先想清楚校验在哪**。
4. **SQLite 时间是 UTC**：`datetime('now')` 存 `"YYYY-MM-DD HH:MM:SS"`。
   任何时间显示**必须**走 `lib/format.ts` 的 `dbTimeToDate()`，直接 `new Date()` 会偏移 8 小时。
5. **LIKE 必须配对**：`escapeLike()` 生成的反斜杠只有搭配 `ESCAPE '\'` 才有效，两者缺一不可。
6. **虚拟列表行用 `top` 定位，不用 transform**：行内的悬停气泡/图片预览是 `position: fixed`，
   transform 定位会破坏它们相对视口的计算。
7. **气泡动画**：向上翻转（`flipUp`）的气泡只能挂 `clip-fade-in`（仅 opacity）。
   `clip-pop-in` 结束时 `transform: none` 会覆盖内联的 `translateY(-100%)`，气泡会跳位。
8. **框选是"累积制"**：一次拖拽中滚轮翻页划过的条目全部计入，多次拖拽并入选中集合；
   空白处单击才清空。改 `useRubberBandSelect` 的 `onApply` 语义会连带影响
   `ContinuousPasteBar` 的计数和 `ClipboardItem` 的徽标。
9. **拖拽结束的那次 click**：由 `ClipboardList` 的 `onClickCapture` 消费 `didDragRef` 拦截。
   动列表的点击逻辑时，框选模式的这段必须一起看。
10. **Escape 键是分层的**：搜索框（仅聚焦时清空）、各对话框（关闭自己）、设置面板
    （先取消录制 → 再关清空确认 → 最后关面板）。**不要把 Escape 处理搬到全局**，
    会一次按键触发两个动作（代码注释里已写明这是踩过的坑）。
11. 应用以 **requireAdministrator** 运行，自启动走**任务计划程序**，不是注册表 Run 键。

---

## 6. 已知风险与技术债（按优先级）

### P1 · 建议下个迭代处理

1. **`ClipboardList.tsx:13` 全量订阅 store**
   ```ts
   const { items, isLoading } = useClipboardStore();   // 无 selector
   ```
   `loadItems` 每次自增 `requestId`，于是每次查询至少触发 2 次 ClipboardList 整体重渲染
   （`setLoading(true)` + `setItems`）。这与 `useDatabase.ts` 注释里"刻意在订阅处用
   `getState()` 避免随 requestId 重渲染"的设计**自相矛盾**。
   修法：拆成两个带 selector 的订阅。改动小、风险低。

2. **原生 `alert` / `window.confirm` 仍在用**（`SettingsPanel` 3 处：热键注册失败、裸键录制、
   连续粘贴热键冲突、存储位置变更/恢复确认）。
   `Dialogs.tsx` 的注释明确写了"替代 WebView2 下渲染残缺的原生 prompt"，但设置面板没跟上。
   修法：接 `ConfirmDialog` / `PromptDialog`，或做一个行内错误态。

### P2 · 有空再改

3. ~~空状态不分场景~~ **已解决（2026-09-29 全局观感迭代）**：`ClipboardList` 空状态按
   搜索无结果（含「清空搜索」按钮）/ 收藏为空 / 空分组 / 无记录 四种场景分文案；
   `SearchBar` 增加了 store→输入框的反向同步（外部清空搜索词时输入框跟随清空）。
4. **启动重复 `load_settings`**：`App.tsx`、`useDatabase.ts`、`ContinuousPasteBar`(进入框选模式时)
   各调一次，设置面板打开时再一次。功能无碍，是冗余 IPC。
5. **组件层零测试**：`@testing-library/react` 只用于 `useDebounce` 的 `renderHook`。
   UI 改动目前**只能人工自测**——这是"UI 改一次要人工验收一轮"的根因。
6. **`StatusBar` 每 10 秒查一次 DB 体积**（`pragma_page_count`）。频率不高但每次都是 IPC+SQL。

### P3 · 清理项（不要顺手删，走单独提交）

7. `src/assets/react.svg` 无人引用（Vite 模板残留）；`public/vite.svg` 被 index.html 引用。
8. 前端 `package.json` 的 `@tauri-apps/plugin-opener` **JS 包未被使用**——打开链接走的是
   后端 `open_external_url`（https + 域名白名单）。Rust 侧 `tauri_plugin_opener` 在
   `commands/update.rs` 里确实用到了，**别去动 Cargo.toml**，只可能清前端依赖。
9. 后端 3 个无人调用的命令见 §4.1。

---

## 7. 标准动作清单

### 新增一个设置项
1. `types/settings.ts`：`AppSettings` 加字段 + `DEFAULT_SETTINGS` 加默认值。
2. `models/settings.rs`：struct 加字段 + `#[serde(default = "default_xxx")]`
   （bool 必须指定函数，理由见 §4.3）+ `impl Default` 同步。
3. `SettingsPanel.tsx`：加 UI，走 `saveNow({ ...settingsRef.current, 新字段 })`。
   **读值一律用 `settingsRef.current` 而不是 `settings` state**（回调闭包会读到旧值）。
4. 若该设置影响列表查询，记得同步 `useClipboardStore.setMaxItems` 之类的影响面。

### 新增一个后端命令
1. Rust：`commands/*.rs` 加 `#[tauri::command]` → `lib.rs` 的 `generate_handler!` 注册。
2. 前端：`invoke<返回类型>('命令名', { camelCase 参数 })`；返回类型在前端补一个 interface。
3. 若需要主动推送，加事件并在组件 `useEffect` 里 `listen`，**返回 `unlisten` 清理**
   （现有写法是 `return () => { unlisten.then(fn => fn()); }`，照抄即可）。

### 改动任何视觉
**先出可对比的静态稿或只改一处，改完给用户看，再继续。** 这是本项目用一天工作量换来的
教训（`feat/ui-polish` 一次上 14 项视觉优化，实测后整支分支被删）。范围清单式拍板对功能
缺陷有效，对**观感**无效。

---

## 8. 验证手段

```bash
npx tsc --noEmit          # 类型检查，零错误
npx vitest run            # 4 文件 20 用例，全绿
npm run build             # tsc && vite build
npm run tauri dev         # 完整应用（唯一能看到真实列表的方式）
npm run dev               # 纯前端 http://localhost:1420，invoke 全失败、列表空
                          #   但面板宽度 = 窗口宽度，适合拖窗口模拟三档断点
```

- **响应式断点用容器查询**（不是视口查询）：`globals.css` 定义
  `--container-narrow: 22.5rem (360px)` / `--container-wide: 32.5rem (520px)`，
  `App.tsx` 根节点挂 `@container`，组件用 `@max-narrow:` / `@min-wide:` 变体。
  面板宽度只有 320–600px，视口断点永远不会触发。
- **构建/发版**：`vite.config.ts` 的 `emptyOutDir` 刻意设为 `false`（本机 safe-delete 垫片
  会拦截 vite 的批量删除导致构建失败）。**发版前必须手动清 `dist/`**，否则旧产物残留。
- **契约自检**（改完命令/事件跑一遍）：
  ```bash
  # 前端调用了哪些命令
  grep -rhoE "invoke(<[^>]*>)?\(\s*'[a-z_]+'" src | grep -oE "'[a-z_]+'" | tr -d "'" | sort -u
  # 后端注册了哪些
  grep -A60 generate_handler src-tauri/src/lib.rs | grep -oE "::[a-z_]+," | tr -d ':,' | sort -u
  ```
  两边 diff，前端多出来的就是悬空调用。

---

## 9. 过期文档警示 ⚠️

工作区根目录的 **`UI-自测清单.md` 已过期，不要照它验收当前版本**。
它描述的是已废弃的 `feat/ui-polish` 分支（提交 `9ad48f2`，该分支已整支删除）。

清单里写了但**当前 master 上不存在**的项：
- 搜索/收藏/分组的分场景空状态 + 「清空搜索」按钮
- 用应用内弹窗替代 `alert` / 原生 confirm（当前仍是原生）
- 设置面板六分区重构（当前是扁平列表，无分区标题）
- 时间 hover 显示绝对时间 tooltip（当前无 title 属性）
- 图片 meta 行"没有 MB 大小"（**当前恰恰有** `formatBytes`）
- 图片预览"不跟随鼠标"（当前是跟随鼠标的）

清单里**仍然有效**的部分：跑起来的两种方式、`npm run dev` 用来模拟断点的技巧、
以及 `npm run build` 前要手动清 dist 的提醒。
