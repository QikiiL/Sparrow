# 粘贴后保持打开(keep-open)设计说明

## 需求

右上按钮区新增一个切换按钮:开启后,单击剪贴板条目不再自动隐藏(销毁)面板,
方便对着同一个目标窗口连续粘贴多条内容;关闭时行为与旧版完全一致。

## 行为定义

- **默认关闭**,状态持久化到 settings.json(`keep_open_on_paste` 字段),
  与隔壁「置顶」按钮同为即改即存的持久开关。
- **关闭(默认)**:单击条目 → 销毁窗口 → 焦点还给**冻结落点** → 模拟 Ctrl+V。
- **开启**:单击条目 → **窗口不销毁**,仅把前台焦点还给目标窗口 → 模拟 Ctrl+V →
  面板留在屏幕上(配合默认开启的「置顶」即悬浮在目标窗口之上),可直接点下一条。
- 「仅复制」模式(状态栏切换):只写剪贴板,不模拟按键,且**绝不**归还焦点
  (`SetForegroundWindow`)。面板是否关闭跟随 keep-open 开关 —— keep-open 关时
  面板照旧关闭(保留"点一下有反应"的观感),keep-open 开时面板保持打开。
- 连续粘贴(框选模式):keep-open **开启**时「开始连续粘贴」**保留面板**,只把
  焦点交给冻结落点(与单击同一条 `focus_target_keep_window` 交接),每一步走
  `FocusHandling::KeepOpen`;refocus 失败(或冻结不到落点)仍降级为销毁面板的
  旧路径,保证连贴一定能离开面板。keep-open 关闭时行为不变(开始时即销毁面板)。
  触发键的拦截改为 **keydown/keyup 配对**(`utils/paste_hook.rs`):只有"键从
  抬起变按下"才出队一条,按住不放的系统自动重复被吞掉,快速连按不再被原来的
  180ms 时间窗去抖吞掉;keyup 丢失时靠 1500ms 卡键自愈恢复。

## 关键取舍:粘贴落点的确定与降级

保持打开意味着模拟 Ctrl+V 时本面板可能仍持有前台焦点——按键会落进自己的
搜索框而不是目标应用。因此 KeepOpen 路径在投递前必须先把前台切到落点:

1. **首选最近一次外部前台窗口**(`utils/foreground_tracker.rs` 的
   `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` 跟踪器记录):面板默认置顶且
   失焦不隐藏,"唤出面板 → 点另一个应用的输入框 → 回来点条目"是常规操作,
   用户真正想去的地方是**点击条目那一刻**最后一次用过的外部窗口,而不是
   唤出瞬间的前台;
2. 跟踪器**明确记录了"没有落点"**(用户点了桌面/任务栏/任务视图,或记录的
   窗口已关闭、被遮蔽、变成不可激活窗口)时,冻结结果是 **`None`**:
   **本次只复制、不激活任何窗口**,由用户手动 Ctrl+V。注意这里**不能**回退到
   唤出时捕获的目标 —— 那正是"点完桌面又被拉回几分钟前的旧应用"的来源
   (跟踪器因此是三态的:`Unknown` 才允许回退,`NoTarget` 一律按"无落点"处理);
3. 只有跟踪器**从未观察到任何前台事件**(冷启动,`Unknown`)时,才退回
   **热键唤出那一刻捕获的目标窗口**(`hotkey::show_with_paste_target`);
4. 冻结结果是 **`Some(0)`** —— 只出现在"没有任何记录"(应用启动即建窗、
   托盘/命令打开)这条老路径上 —— 时,KeepOpen 退回 **Z 序上本面板下方第一个
   可激活的普通层窗口**,HideFirst 靠销毁窗口后的隐式激活。这正是"销毁窗口"
   路径里 Windows 隐式激活会选择的对象,保证落点与"关掉开关"时的行为一致。
   Z 序回退的候选过滤直接复用 `foreground_tracker::is_valid_target`,跳过:
   置顶窗口(面板自身默认置顶,悬在置顶层,紧挨其下是任务栏等置顶壳窗口;
   粘贴目标在普通层)、UWP 幽灵窗口(`IsWindowVisible` 为真但被 DWM cloaked)、
   本进程窗口(按键会打回自己)、`WS_EX_NOACTIVATE` 覆盖层(接不了键盘输入)、
   禁用窗口;
5. **全部失败**(桌面上没有其他可激活窗口等)才降级为"仅复制":
   内容已写入剪贴板、面板保持打开,用户手动 Ctrl+V 即可。

> `None` 与 `Some(0)` 必须在**调用方**也区分开:冻结结果一路是
> `Option<isize>`,若在边界塌成单个 `isize`,`None`(点了桌面)就会掉进
> KeepOpen 的 Z 序兜底,变成"粘进任意一个下方窗口"—— 正是本 bug 的变体。

落点在**点击条目那一刻**一次性冻结(`foreground_tracker::freeze_paste_target`),
之后的窗口销毁/焦点交接/异步等待阶段**只认这个冻结值并按参数传递**,既不读
跟踪器的新记录,也**不写回** `PasteMode`(写回会把"唤出瞬间捕获值"污染成上次
粘贴的应用,使跟踪器失效时的回退一路错到底)。

发送按键前还有最后一道闸(`input_focus::wait_for_paste_target`):前台必须
就是冻结的落点(最多再等约 300ms);落点为 0 时要求前台通过
`foreground_tracker::is_acceptable_paste_foreground` —— 它比 `is_valid_target`
宽一档,**放行桌面**(`Progman`/`WorkerW`,「框选文件 → 连贴到桌面」是合法用法),
但仍排掉任务栏/任务视图/`WS_EX_NOACTIVATE` 覆盖层/cloaked/本进程窗口。
不满足就**不发** Ctrl+V —— 剪贴板内容照常写入,按"仅复制"处理。宁可少粘一次,
也不把内容粘进错误的软件。

> 实测教训:初版没有第 4 层退回,凡是不经热键唤出的面板(启动即建窗、
> 托盘打开)目标句柄为 0,直接落入降级分支,表现为"点击条目毫无反应"。
> 第二版补上 Z 序回退后仍无反应——回退取到的是任务栏:面板默认置顶、
> 悬在 Z 序置顶层,紧挨其下的下一个可见窗口就是任务栏,焦点"成功"切过去、
> Ctrl+V 粘进不可见的任务栏。第三版起 Z 序回退只认普通层窗口。
>
> 第四版(2.1.x)补上第 1 层:此前落点只在热键唤出时捕获一次、此后从不刷新。
> 面板置顶 + 失焦不隐藏让"唤出后再点别的应用的输入框"成为常规操作,陈旧的
> 捕获值会把前台抢回**上一个**软件(浏览器网页输入框、微信聊天窗口最常见的
> 报障形态),HideFirst 与 KeepOpen 两条路径同病。
>
> 同版审查补丁:跟踪器改三态,否则"点桌面"被当成"从未跟踪"而回退到陈旧捕获值,
> 症状照样复现;`WS_EX_NOACTIVATE` 覆盖层与 Win11 壳窗口类名一并纳入"无落点";
> 前台事件回调里不再做 DWM 查询(目标进程挂起时会阻塞消息泵)。


## 实现

- `models/settings.rs`:`AppSettings.keep_open_on_paste: bool`,`#[serde(default)]`
  兼容旧配置;默认 `false`。
- `utils/foreground_tracker.rs`:进程级跟踪器(`AtomicIsize` + `AtomicU64`),
  **三态**编码(0 = `Unknown` 冷启动;-1 = `NoTarget` 壳/桌面/不可激活/已失效;
  > 0 = 窗口句柄),`SetWinEventHook` 专用线程 + `GetMessageW` 消息泵;
  装不上钩子时降级为 40ms 轮询。记录前过滤(只用不跨进程阻塞的查询):
  `IsWindow`/`IsWindowVisible`、非本进程(pid 双保险)、非 `WS_EX_NOACTIVATE`、
  非壳窗口(`Shell_TrayWnd`/`Shell_SecondaryTrayWnd`/
  `Windows.UI.Core.CoreWindow`/`XamlExplorerHostIslandWindow`/
  `Shell_CharmWindow`/`MultitaskingViewFrame`)与非桌面窗口
  (`Progman`/`WorkerW`);**壳/桌面/不可激活窗口 → 置 `NoTarget`** 而不是忽略
  (点完桌面不该再拉回几分钟前的应用),本进程窗口直接忽略(不覆盖上一条有效
  记录);**瞬态壳浮层**(`NotifyIconOverflowWindow`/
  `TopLevelWindowForOverflowXamlIsland`,即托盘溢出面板)也**忽略** ——
  它们点完就隐藏,记成落点会让"从溢出区打开面板 → 点条目"退化成只复制;
  cloaked(DWM)检查推迟到读取/冻结时做,不进前台事件回调(目标进程挂起
  时会阻塞消息泵)。`Windows.UI.Core.CoreWindow` 被全屏 UWP 应用共用,只有
  宿主进程是壳宿主(`StartMenuExperienceHost`/`SearchHost`/`SearchApp`/
  `ShellExperienceHost`)时才判成壳 —— 进程名用
  `OpenProcess`+`QueryFullProcessImageNameW` 取,不发消息、不阻塞。
  `choose_paste_target` / `decide` / `apply` 是纯逻辑,带单测。
- `services/paste_service.rs`:投递核心 `deliver_item_by_id` 的 `hide_first: bool`
  参数升级为 `FocusHandling` 枚举(`HideFirst` / `KeepOpen` / `AtTarget`),
  三种焦点策略集中在同一处,单击与连续粘贴继续共用一条投递路径;函数入口
  第一件事就是冻结落点(早于任何窗口操作与异步等待),冻结值全程按参数传递,
  不写回 `PasteMode`。冻结结果是 `Option<isize>`:`None` → 本次只复制
  (`do_paste` 被降级,KeepOpen 保持面板、HideFirst 只销毁面板),`Some(0)` →
  老路径(Z 序兜底/隐式激活)。KeepOpen 分支用 `refocus_target_keep_window`
  (不销毁窗口版的 `release_focus_to_target`)归还焦点,`target != 0` 时**不做**
  Z 序兜底,失败即降级。`deliver_content` 的 `DeliverMode::Paste { target }`
  携带冻结落点,并在 `simulate_paste` 前做前台校验;返回值 `Ok(pasted)` 把
  "是否真的发了按键"带给调用方,`Delivery::Delivered { pasted }` 如实上报
  (连续粘贴在 `pasted == false` 时不标记「已粘贴」)。
- `commands/clipboard.rs::activate_item`:点击时读设置选择 `KeepOpen` 或 `HideFirst`
  (设置经 tauri-plugin-store 内存缓存,点击路径无磁盘开销)。
- `commands/settings.rs::set_keep_open`:照搬 `set_always_on_top` 的即改即存模式。
- 前端 `App.tsx`:右上按钮区(置顶按钮右侧)新增切换按钮,图标为窗口轮廓
  (`AppWindowIcon`),高亮态与置顶按钮同款 accent 样式;类型定义
  `types/settings.ts` 同步加字段(设置面板整体回传设置对象,前后端必须同时有)。

## 已知边界

- 面板未置顶时,粘贴后目标窗口成为前台,可能盖住面板;面板并未关闭,
  热键再按一次即回到前台(热键对可见窗口的语义是隐藏,见 hotkey.rs)。
  置顶(默认开)时不存这个问题。
- 连续快速点击:每次点击都会重新归还焦点,与单次点击同一条路径,无需额外排队。
- 「仅复制」模式(force_paste=false)不 `SetForegroundWindow`、不归还焦点:
  只写剪贴板,焦点留在用户原处。面板是否关闭跟随 keep-open(关 → 照旧关闭,
  开 → 保持打开)。
- **最坏延迟预算**:一次"校验失败→只复制"的点击 ≈ 销毁面板(百毫秒级)+
  焦点轮询 300ms + 写剪贴板 + 50ms + 发送前闸门 300ms ≈ **650ms** 才会
  判定"只复制"。成功路径远快于此(第一拍通常就命中,≈ 100ms 量级)。
- 连续粘贴遇到"只复制":队列照常推进(条目已出队)、剪贴板里有内容,但**不**
  标记「已粘贴」—— UI 的 `pasted` 计数会小于 `total`,横幅可能显示
  "已粘贴 N-1 条"。这是如实反映"那一条没粘出去",不是计数 bug。
- **框选文件 → 连贴到桌面**:桌面(`Progman`/`WorkerW`)在"无明确落点"路径里
  是**允许**的前台(否则这个用法会被闸门拦死),所以连贴文件到桌面仍然生效;
  而连贴**文本**到桌面时目标应用(桌面)本身不处理粘贴,表现为"没反应" ——
  与改动前一致。
- **托盘溢出区**(Win11 默认把托盘图标收进溢出面板):`NotifyIconOverflowWindow`
  (Win10)与 `TopLevelWindowForOverflowXamlIsland`(Win11)是 explorer 的瞬态
  可激活浮层,点完托盘图标就隐藏、句柄随即失效。跟踪器对它们**忽略**(既不当
  落点、也不清空记录),所以"从溢出区打开面板 → 点条目"仍按用户原来的应用
  粘贴,不会退化成只复制。
- **单击路径的"只复制"没有 UI 反馈**:闸门拦下按键时内容已进剪贴板,但面板
  该关还是关、界面没有任何提示(release 版也没有 stderr);用户若觉得"点了
  没反应",手动 Ctrl+V 即可。要提示需前端加 toast(本次未改 UI)。
- **拿不到进程名时按"非壳"处理**:`owning_exe_name` 失败(权限不足/进程已
  退出)会让 `Windows.UI.Core.CoreWindow` 被当成普通窗口 —— 方向安全
  (可粘,不会错粘到任务栏)。
- **KeepOpen 失败会重试两条路径**:`refocus_target_keep_window` 失败后还会走
  一次 `release_focus_to_target`(销毁面板 + 再等 300ms),最坏多花约 1s 才
  落到"只复制"。正常路径不受影响。
- Win32 焦点语义的固有限制:目标进程若正被系统前台锁定(UWP 挂起、权限
  更高的窗口、正在拖拽),`SetForegroundWindow` 可能只闪任务栏;此时发送前
  校验会拦住按键,表现为"只复制了"(设计上宁可少粘一次也不粘错地方)。
- 真机验收必须覆盖:全屏 UWP 应用(照片/计算器)里的输入框(确认没被误判成
  壳窗口)、Win+Tab 任务视图后回面板点条目(应只复制)、点任务栏后点条目
  (应只复制)、**从托盘溢出区打开面板后点条目**(应正常粘贴)、框选文件连贴
  到桌面(应成功)。


