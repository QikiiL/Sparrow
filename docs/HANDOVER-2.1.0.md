# 交接文档：剪雀 Sparrow 2.1.0（云同步）

面向接手 2.1.0 发布与后续维护的人。读完应能独立完成：构建、双设备验收、发版，并知道这一版**刻意没做什么**。

---

## 1. 版本与仓库

| 项目 | 值 |
| --- | --- |
| 版本 | **2.1.0** |
| 分支 | `feat/cloud-sync` |
| HEAD | `81ae7a8`（`fix: store sync blobs as .dat for Koofr WebDAV`） |
| 目标 Release | `v2.1.0` |
| 分支 URL | https://github.com/QikiiL/Sparrow/tree/feat/cloud-sync |
| 版本文件 | 仓库根 `version.json`（应用内更新检查读它） |

2.1.0 的核心增量只有一个：**WebDAV / S3 兼容对象存储的端到端加密云同步**。其余改动都是围绕它的加固与修复（见第 5 节）。

---

## 2. 交付内容

### 2.1 架构：无中心服务器、无 P2P

每台设备**各自**往同一个远端目录写一份自己的快照，读取时把所有 peer 快照拉下来合并。没有服务端、没有中继、没有 P2P 打洞——用户自己的网盘/桶就是唯一的数据中转。

远端布局（`{root}` 是用户配置的根路径）：

```
{root}/
  meta.json              # 明文：KDF 参数 + 16B salt + 密钥校验值 check
  devices/{id}.json      # 每设备一份加密快照（{id} 为设备 ID）
  blobs/{hash}.dat       # 内容寻址的加密附件（图片、富文本原始字节）
```

`meta.json` 是全库唯一的引导文件：**先有它才能派生密钥**。它只在第一次同步时用 `If-None-Match: *` 创建；若两台设备用不同 salt 各建一份，后到的那台会因 `check` 不匹配而显式报错（"同步口令与远端不一致"），不会静默产生两份互不兼容的数据。

### 2.2 后端

| 后端 | 实现文件 | 说明 |
| --- | --- | --- |
| WebDAV | `services/sync/webdav.rs` | 通用 WebDAV（坚果云、Koofr、Nextcloud 等） |
| S3 兼容 | `services/sync/s3.rs` + `services/sync/sigv4.rs` | 自实现 SigV4 签名，不依赖 AWS SDK |

两者都实现同一套 `backend.rs` trait（get / put / delete / list / exists），上层同步逻辑与后端无关。

### 2.3 加密参数

| 项目 | 值 |
| --- | --- |
| 密钥派生 | Argon2id，`m = 65536`（64 MiB）、`t = 3`、`p = 1`，输出 32B 密钥 |
| salt | 16B 随机，base64 存于 `meta.json`（明文） |
| 对称加密 | AES-256-GCM，12B nonce，16B tag |
| AAD | **对象 key 本身**（如 `devices/dev-1.json`、`blobs/<hash>.dat`）——换 key 重放必然解密失败 |
| 信封格式 | `{"v":1,"alg":"A256GCM","nonce":"<b64>","ct":"<b64>"}` |
| 密钥校验值 | `meta.check = hex(HMAC-SHA256(key, "sparrow-sync-meta-check-v1"))`，用于开同步前判定口令是否一致 |
| 派生缓存 | 口令 + salt 指纹命中时跳过 Argon2id（省 0.1–0.3s），但**仍然**校验 `check` |

附件 blob 在信封外还带 5 字节魔数 `SPRW1`，用于最短长度与格式校验。

### 2.4 凭据

口令与后端凭据（WebDAV 密码 / S3 AK+SK）全部存 **Windows 凭据管理器**，服务名 `Sparrow.Sync`（`services/credentials.rs`）。`settings.json` 里**只有**非敏感配置（根路径、后端类型、轮询间隔、同步模式等），任何密钥都不会落盘到配置文件或日志。

---

## 3. 同步语义

| 语义 | 实现方式 |
| --- | --- |
| 冲突解决 | **LWW by `updated_at`**：同一内容 hash 在多份 peer 快照中出现时，取 `updated_at` 最大的一条 |
| 去重 | 内容 SHA-256 hash 相同即视为同一条，不产生重复行 |
| 删除 | **墓碑（tombstone）**：显式删除或清空历史时写入 `deleted_at`；`deleted_at >= updated_at` 判为已删除（**平手也判删除**）；本地更晚的编辑可"复活"该条 |
| 墓碑保留 | 60 天（`TOMBSTONE_TTL_MS`），覆盖单设备离线两个月的场景。**保留策略本身不传播**——各设备按自己的时钟清理 |
| 待写墓碑 | 仅启动时模式下墓碑先落本地 `sync-tombstones.pending`，下个周期补传，避免"删了又回来" |
| 模式 `interval`（默认） | 按 `poll_interval_sec` 轮询（2–300s），本机复制/删除事件立即唤醒 |
| 模式 `startup` | 每次启动只跑一轮，忽略剪贴板事件 |
| 拉取行为 | **只写本地 DB 与磁盘附件，绝不写系统剪贴板**——同步来的内容不会顶掉你正在复制的东西 |
| 图片 | 落 `data/images/`，文件名取 hash 前 16 位（与 `clipboard_monitor::save_image_to_disk` 一致），不是 blob key |
| 富文本 | 落 `data/formats/`，CF_HTML / RTF **原始字节无损**保存；文本仍是权威正文 |
| 不同步 | 文件列表（CF_HDROP）、超限内容、被敏感内容闸门拦下的条目 |

上传前会**再查一次**敏感内容闸门（两道闸门 + 全量加密），避免配置变更后旧内容被顺带传出去。

---

## 4. 关键文件表

### Rust

| 文件 | 行数量级 | 职责 |
| --- | --- | --- |
| `src-tauri/src/services/sync/service.rs` | ~3400 | 同步主循环：周期编排、唤醒、上传/下载、进度、诊断。**改动最集中，先读它** |
| `src-tauri/src/services/sync/state.rs` | ~770 | 本地同步状态（`sync-state.json`）：`last_upload_hash`、`uploaded_blobs`、墓碑、待写墓碑、`blob_key_version` |
| `src-tauri/src/services/sync/snapshot.rs` | ~1020 | 本机快照的构建与解析（条目 ↔ `SyncRecord`） |
| `src-tauri/src/services/sync/merge.rs` | ~300 | 纯函数合并：LWW + 墓碑裁决，输出 `MergeAction` 计划 |
| `src-tauri/src/services/sync/crypto.rs` | ~650 | `meta.json` 引导、Argon2id、AES-GCM 信封、blob 加解密、`check` 校验 |
| `src-tauri/src/services/sync/webdav.rs` | ~720 | WebDAV 后端（含 MKCOL 建目录、HEAD/GET/PUT/DELETE） |
| `src-tauri/src/services/sync/s3.rs` | ~920 | S3 兼容后端 |
| `src-tauri/src/services/sync/sigv4.rs` | ~260 | SigV4 签名 |
| `src-tauri/src/services/sync/dns.rs` | ~130 | DNS 解析（3s 超时）+ 中文可诊断错误文案 |
| `src-tauri/src/services/sync/backend.rs` | ~60 | 后端 trait 定义 |
| `src-tauri/src/services/sync/e2e_tests.rs` | ~1070 | 双设备端到端测试（内存后端模拟两台机器） |
| `src-tauri/src/commands/sync.rs` | ~215 | IPC：`get_sync_status`、`sync_now`、`set_sync_secret`、`has_sync_secrets`、`test_sync_connection` |
| `src-tauri/src/models/sync.rs` | ~300 | `SyncSettings`、`SyncMode`、`SyncStatus` 等类型 |

### 前端

| 文件 | 职责 |
| --- | --- |
| `src/components/SyncSection.tsx` | 设置面板「云同步」区块：后端选择、字段、测试连接、立即同步、状态与告警 |
| `src/hooks/useSyncStatus.ts` | 状态订阅：先注册监听再取初始值，同步中每 2s 轮询 `get_sync_status` |

### 文档

| 文件 | 说明 |
| --- | --- |
| `docs/CLOUD-SYNC.md` | 面向用户的配置与验收手册（配置项总表、安全模型、双设备步骤）。改行为必须同步更新它 |
| `docs/HANDOVER-2.1.0.md` | 本文件：面向接手者的实现与发布说明 |

---

## 5. 本版修复时间线

按时间顺序，一句话概括：

| 提交 | 说明 |
| --- | --- |
| `b008690` | 修复带连字符的 `sk-` API 密钥检测（正则只允许字母数字，导致含 `-`/`_` 的密钥被截断而漏检）——与本版云同步同批发布，属独立修复 |
| `c2d3c85` | 云同步主体落地：WebDAV/S3 后端、每设备加密快照、Argon2id + AES-256-GCM、hash 去重、墓碑、图片 blob、凭据进凭据管理器 |
| `563169d` | 富文本（CF_HTML/RTF）按原始字节同步、图片跳过原因显式化与 0 字节自愈、新增「仅启动时」模式并把墓碑持久化到本地待写日志 |
| `fbd7619` | 网络加固：DNS 失败给可执行的中文提示、GET 校验长度 + `Connection: close` 非池化、重试增至 5 次（500ms/2s/5s/10s）、坏 peer 跳过并冷却 60s、测试连接改为上传 256KB 以真正压到数据面 |
| `61fd993` | 修复自唤醒死循环与卡在「同步中」：被跳过的图片/富文本不再 emit `clipboard-changed`（同步自己会把它当成本地变更再次唤醒），同步自身产生的事件按 payload 忽略；状态 hook 改为先注册后取初值 + 同步中 2s 轮询 |
| `1d83af3` | DNS 3s 超时（校园网 DNS 污染不再让整轮挂死）、`SyncStatus` 增加 phase 让 UI 显示连接/合并/条目/blob 进度、blob GET/PUT 20s 超时 × 2 次尝试、附件上传失败降级为告警而非中断整轮 |
| `81ae7a8` | **Koofr 附件不同步的根因修复**：blob 对象改用 `.dat` 后缀；同步状态版本升到 2，一次性清空 `uploaded_blobs` 与 `last_upload_hash` |

### 5.1 重点：Koofr `.bin` 空 body 根因

现象很反直觉，值得记住，否则以后会重复踩：

| 观察 | 结果 |
| --- | --- |
| `GET blobs/{hash}.bin` | **HTTP 200 + `Content-Length` 正常**，但 body 实际 0 字节 |
| `HEAD blobs/{hash}.bin` | 正常（大小正确） |
| `GET devices/{id}.json` | 正常，所以 JSON 快照能同步 |
| `GET blobs/{hash}.dat` | 正常 |

也就是说：**Koofr 只对 `.bin` 这个扩展名做了特殊处理**，返回一个"声称有内容但实际为空"的 200 响应。结果是每张图片、每份富文本下载都失败，而快照同步看起来完全正常——表象像"附件加密/解密坏了"，实际是后端 CDN 行为。

修复方式：blob 对象统一用 `.dat`（`blobs/{hash}.dat`）。同时 `state.rs` 里 `BLOB_KEY_VERSION` 升到 2，老状态（无该字段或版本更低）在加载时**一次性清空** `uploaded_blobs` 和 `last_upload_hash`：

- 不清空的后果：`uploaded_blobs` 会认为"这个 hash 已经传过了"而跳过上传，但旧记录指向的 `.bin` key 已经没人读，附件就永远补不上。
- 清空的代价：首次同步会把全部附件重新上传一遍（约 28MB，属正常现象，已在版本说明里写明）。
- 该迁移是幂等的：只触发一次，之后版本号相等不再清空。

---

## 6. 构建与验证

以下为 2.1.0 发布构建的实测结果：

| 项目 | 命令 | 结果 |
| --- | --- | --- |
| Rust 单测 | `cd src-tauri && cargo test --lib` | **246 passed / 0 failed / 4 ignored** |
| 前端单测 | `npx vitest run` | **66 passed** |
| 类型检查 | `npx tsc --noEmit` | **0 error** |
| 生产构建 | `npm run tauri build` | **exit 0** |

产物与校验：

| 产物 | 大小 | SHA-256 |
| --- | --- | --- |
| `src-tauri/target/release/sparrow.exe` | 28,180,992 B | `8d4d1f08d0f42c821e2c8bb978ed63af700b8cf731dd9ce84b53299067c9b3a7` |
| `src-tauri/target/release/bundle/nsis/Sparrow_2.1.0_x64-setup.exe` | 7,241,417 B | `14adab63d4f9897b78920df63a021166960649da7f4bb1ca2c3c056d193e583a` |

注意：`cargo test`（不带 `--lib`）在提权环境外会因 `build.rs` 给 bin target 链接了 `requireAdministrator` manifest 而报 os error 740。**只跑 `--lib`**，这是设计如此，不是回归。

---

## 7. 验收步骤（双设备）

前提：两台设备都能访问同一个 WebDAV 目录或 S3 桶；口令（同步口令）由你自定，两端**必须一致**。

1. **退出旧进程**：两台设备都先从托盘「退出」旧版（确认任务管理器里没有残留 `sparrow.exe`），否则旧进程会继续写库。
2. **安装 2.1.0**：直接覆盖安装。安装器会读取旧版卸载项记录的安装位置并原地升级，`config/`、`data/`、历史记录全部保留。
3. **先让附件多的那台同步**：进入 设置 → 云同步，填后端与根路径、设置同步口令，点「测试连接」（会真上传 256KB，能压到数据面），然后「立即同步」。观察进度到 **上传附件 i/n** 阶段并跑完。这一轮会重传全部附件（约 28MB），耗时取决于上行带宽。
4. **另一台再同步**：同样配置后「立即同步」，此时应看到 **同步条目 i/n** 以及附件下载进度。
5. **检查落地**：
   - `data/images/` 下出现新图片文件；
   - `data/formats/` 下出现富文本副本（CF_HTML / RTF）；
   - UI 状态条**无告警**。若显示"N 张图片未同步 / N 个富文本未同步"，说明有附件失败，看日志里的具体原因（跳过原因已显式化）。
6. **校园网 DNS 问题**：若报 DNS 相关错误（本版有 3s 超时和中文提示），用 hosts 固定后端域名，或给系统配代理；不要在 DNS 被污染时反复重试。
7. **旧 `.bin` 清理**：远端 `blobs/` 下残留的旧 `.bin` 对象已无人读取，可手动删除以省空间（不删也不影响功能）。

---

## 8. 安全与已知限制

### 8.1 安全模型

- 远端只存**密文**：快照、附件、以及 `meta.json` 之外的一切。`meta.json` 明文但只含 KDF 参数、salt、`check`，泄露它不足以解出密钥。
- 口令与后端凭据只在 Windows 凭据管理器（`Sparrow.Sync`），不进 `settings.json`、不进日志；日志里的 URL 与凭据已脱敏。
- AAD 绑定对象 key，跨对象重放/替换会导致解密失败。

### 8.2 已知限制（**刻意未做**，不要当成 bug）

| 限制 | 影响 |
| --- | --- |
| 本地 DB 与设置**仍未加密** | 加密只发生在同步层。拿到本机磁盘的人可以直接读 `data/clipboard.db` |
| 同一口令 = 同一信任域 | 没有按设备/用户隔离的权限模型；知道口令的人可读全部同步内容 |
| 富文本按原始字节存储/粘贴 | 不净化 HTML/RTF 内容，粘贴目标应用自行处理 |
| 单周期无总超时 | 一轮同步可能因为网络慢而跑很久（单项有超时：DNS 3s、blob 20s×2） |
| blob 串行传输 | 附件逐个传，不做并发 |
| 无断点续传 | 大附件中断需整份重传 |
| LWW 无逐条冲突 UI | 冲突按 `updated_at` 静默裁决，用户看不到"被覆盖了哪一条" |
| Windows-only | 依赖 clipboard-win / winreg / CF_HDROP |
| `version.json` 已是 2.1.0 | 应用内更新检查以它为准，发版前不要再改 |

---

## 9. 后续建议（按性价比排序）

1. **附件并发 + 断点续传**：当前串行且整份重传，是首次同步 28MB 慢的主因。可先做小并发（2–3 路），再做分片续传。
2. **单周期总预算**：给一轮同步加整体时限，超时后留到下轮，避免弱网下"一直在同步"。
3. **远端旧 `.bin` 清理**：可在某轮同步里顺手列一次 `blobs/` 并删除非 `.dat` 对象，省掉用户手动清理。
4. **S3 实测**：S3 路径有单测与 SigV4 覆盖，但真实厂商（MinIO / R2 / B2）兼容性尚未逐家验证，建议补一轮真机验收并记录到 `docs/CLOUD-SYNC.md`。
5. **冲突解决 UI**：LWW 足够日常使用，但需要时至少能列出"本机与远端同 hash 不同 `updated_at`"的条目，供人工确认。

---

## 10. 发布检查清单

- [ ] `version.json`：`version` = `2.1.0`，`github` 指向 `releases/tag/v2.1.0`，`lanzou` / `lanzou_password` 保持原值，JSON 合法且为 UTF-8 无 BOM
- [ ] `docs/HANDOVER-2.1.0.md` 已随分支提交
- [ ] PR：`feat/cloud-sync` → `main`，合并前确认 CI / 本地 `cargo test --lib`、`npx vitest run`、`npx tsc --noEmit` 全绿
- [ ] 打 tag `v2.1.0` 并推送
- [ ] Release asset：上传 `Sparrow_2.1.0_x64-setup.exe`（可同时附 `sparrow.exe`）
- [ ] hash 校验：按第 6 节的两个 SHA-256 与字节数核对上传后的文件
- [ ] 两端复测：按第 7 节完整走一遍双设备验收，确认 `data/images`、`data/formats` 有内容且无状态告警
- [ ] 发布说明：直接复用 `version.json` 的 `notes`，提醒用户首次同步会重传全部附件、两端都要 2.1.0
