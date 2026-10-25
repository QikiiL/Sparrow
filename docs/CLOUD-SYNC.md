# 云同步（WebDAV / S3）：使用与验收

> 适用版本：`feat/cloud-sync` 分支。平台：Windows（凭证管理器为 Windows Credential Manager）。
> 本文只描述已实现的行为；字段名、默认值、限制均取自源码。

---

## 1. 是什么 / 不是什么

**是什么**

- 把 WebDAV 或 S3 兼容对象存储当成一个**共享抽屉**：每台设备把自己的剪贴板条目加密后放进去，其它设备定期去抽屉里取。
- 每台设备**只写自己那一个对象**（`devices/{deviceId}.json`），互不覆盖，因此不需要任何协调服务。
- **端到端加密**：条目内容在上传前用同步口令派生的密钥加密。存储服务方只能看到密文、对象名和对象大小。

**不是什么**

- 不需要自建服务器、不需要账号体系、不经过任何 Sparrow 的中转服务器。
- 没有 P2P、没有实时长连接；本机复制/删除会通过 `clipboard-changed` 事件**立刻唤醒**一轮同步，对端仍靠轮询，所以"从复制到对端出现"最坏约**一个**轮询间隔（默认 5 秒，不再是 2×）。
- 不是备份工具：不保留历史版本，删除会传播（见第 6 节）。
- 不做剪贴板内容的"云端剪贴板"式接管：拉取到的条目只写本地数据库，**不会**写系统剪贴板（避免回环）。

---

## 2. 支持的后端

### 2.1 WebDAV

需要支持 `PROPFIND`、`MKCOL`、`PUT`、`GET`、`HEAD`、`DELETE` 与 `If-Match` / `If-None-Match` 的 WebDAV 服务。
认证方式：HTTP Basic（`用户名` + 密码，密码存在凭据管理器）。

配置里填的是**集合根 URL**，最终路径为：

```
{webdav_url}/{root_prefix}/{key}
```

`root_prefix` 默认 `sparrow`，`key` 是 `meta.json`、`devices/<uuid>.json`、`blobs/<hash>.bin`。
**建议 URL 只写到父目录**，最后一级交给 `root_prefix`，否则会出现 `.../sparrow/sparrow/devices/...`。

> **URL 里不要写用户名/密码。** `https://alice:secret@dav.example.com/x` 这种内嵌 userinfo 的写法会被后端直接拒绝（`WebDAV 地址不要包含用户名或密码，请分别填写用户名和密码`），设置面板也会在失焦时拦下并**不保存**。原因：`webdav_url` 是明文写进 `settings.json` 的，内嵌密码等于把密码落到磁盘上；用户名请填「用户名」，密码请填「密码」（进凭据管理器）。

| 服务 | `webdav_url` 示例 | 说明 |
|---|---|---|
| Nextcloud / ownCloud | `https://dav.example.com/remote.php/dav/files/alice` | 用「应用密码」而不是登录密码；用户名是登录名 |
| 坚果云 | `https://dav.jianguoyun.com/dav` | 必须用「第三方应用密码」；根目录为「我的坚果云」 |
| 群晖 DSM（WebDAV Server 套件） | `https://nas.example.com:5006/clipboard` | `5005` 是 http（会被拒绝），`5006` 是 https；路径是共享文件夹名 |

### 2.2 S3 兼容

需要支持 SigV4 签名、`ListObjectsV2`、条件写（`If-Match` / `If-None-Match`）。
对象路径为 `{root_prefix}/{key}`，不需要预先建目录。

| 服务 | `s3_endpoint` | `s3_region` | `s3_path_style` |
|---|---|---|---|
| AWS S3 | `https://s3.us-east-1.amazonaws.com` | `us-east-1` | 关（virtual-host） |
| MinIO | `https://minio.example.com:9000` | `us-east-1`（MinIO 默认） | **开**（带端口/IP 必须 path-style） |
| Cloudflare R2 | `https://<accountid>.r2.cloudflarestorage.com` | `auto` | **开**（推荐，避免 bucket 子域解析问题） |
| Backblaze B2 | `https://s3.us-west-004.backblazeb2.com` | `us-west-004` | 关 |

注意事项：

- **endpoint 必须带 `https://`**（不带协议时会自动补 `https://`），且不能是 IP 时使用 virtual-host 寻址 —— 代码会直接报错，此时改开 path-style。
- `region` 不能为空，且必须与服务商要求一致，否则签名报 `SignatureDoesNotMatch`。
- bucket 名只允许字母、数字、`.`、`-`、`_`。
- 只填 `s3_access_key`（Access Key ID）；`Secret Access Key` 单独存凭据管理器，**不写进 settings.json**。
- 部分 S3 兼容实现不支持条件写（返回 400/501）：此时并发保护退化为「先 HEAD 再 PUT」，多设备同时写仍有极小概率互相覆盖。

---

## 3. 配置项总表

字段位于 `settings.json` 的 `sync` 对象下（`AppSettings.sync`）。旧配置文件缺 `sync` 字段时整体回落默认值，不会报错。

| 字段 | 类型 | 默认值 | 说明 |
|---|---|---|---|
| `enabled` | bool | `false` | 总开关。关闭时状态为 `disabled`，每 2 秒只读一次内存配置，不碰网络与密钥 |
| `provider` | `"webdav"` \| `"s3"` | `"webdav"` | 后端类型，小写序列化 |
| `webdav_url` | string | `""` | WebDAV 集合根 URL，必须 https（localhost 例外）；query/fragment 会被丢弃；**不能内嵌用户名/密码**（会明文进 settings.json，后端直接报错） |
| `username` | string | `""` | WebDAV 用户名（Basic 认证的用户名部分） |
| `s3_endpoint` | string | `""` | S3 endpoint，含 scheme；不带则补 `https://` |
| `s3_region` | string | `""` | SigV4 签名区域，不能为空 |
| `s3_bucket` | string | `""` | bucket 名 |
| `s3_access_key` | string | `""` | Access Key ID（**只有 ID**，secret 不在这里） |
| `s3_path_style` | bool | `false` | `true` = `{endpoint}/{bucket}/...`；`false` = `{bucket}.{host}/...` |
| `root_prefix` | string | `"sparrow"` | 远端根前缀，去掉首尾空白与 `/`；为空时回落 `sparrow` |
| `poll_interval_sec` | u32 | `5` | 轮询间隔秒。代码夹到 **2–300**；出错后下一轮取 `max(间隔, 30s)`。`sync_mode = "startup"` 时该字段不生效 |
| `sync_mode` | `"interval"` \| `"startup"` | `"interval"` | 同步方式。`interval` = 按 `poll_interval_sec` 轮询；`startup` = 启动跑一轮后**不轮询**，见 6.3 |
| `device_name` | string | `COMPUTERNAME`，取不到为 `"device"` | 设置面板里的「设备名」会写进每次上传的快照（`Snapshot.device_name`），用来在别的设备上认出这条快照；**不是**设备标识（设备标识是 `device_id`）。改设备名只影响展示，不影响快照摘要，因此不会触发额外上传 |
| `sync_images` | bool | `true` | 是否同步图片；关闭后快照里跳过 `type=2` |
| `max_blob_mb` | u32 | `20` | 单张图片上限（MB），超限的图片被跳过并记一行日志 |

补充说明：

- **设置面板暴露的项**：`enabled`、`sync_mode`、`provider`、`webdav_url`、`username`、WebDAV 密码、`s3_endpoint`、`s3_region`、`s3_bucket`、`s3_access_key`、S3 Secret Key、`s3_path_style`、`poll_interval_sec`（仅 `interval` 模式显示）、`device_name`、`root_prefix`、`sync_images`、同步口令。
- **`max_blob_mb` 不在界面里**，只能手改 `settings.json`；改完需要重启应用（轮询循环持有的是内存副本，只有设置面板保存才会替换）。
- 真正的设备标识是 `device_id`（UUID v4），自动生成，存在 `config/sync-state.json`，与 `device_name` 无关。

---

## 4. 凭证与口令

三项敏感值**永远不进 settings.json**，也不出现在任何返回值、错误消息或日志里。

| 用途 | 凭据管理器条目（service = `Sparrow.Sync`） | 环境变量覆盖 |
|---|---|---|
| WebDAV 密码 | account = `webdav_password` | `SPARROW_WEBDAV_PASSWORD` |
| S3 Secret Access Key | account = `s3_secret_key` | `SPARROW_S3_SECRET_KEY` |
| 同步口令（派生加密密钥） | account = `passphrase` | `SPARROW_SYNC_PASSPHRASE` |

- **优先级：环境变量 > 凭据管理器**。环境变量存在且非空时优先使用；空字符串视为未设置。
- 设置面板里输入后点保存（密码/密钥失焦即保存），写入 Windows 凭据管理器；写入空字符串等于删除该条目。
- **清除已保存的凭证**：设置面板「已保存」标记旁有「清除」按钮（WebDAV 密码、S3 Secret Key、同步口令三处都有）。点击后以空值调用 `set_sync_secret`，即从凭据管理器删除该条目，并立刻刷新「已保存」状态 —— 这是界面里唯一能清掉凭证的入口（输入框留空失焦不会删除，避免误清）。
- 可以在「凭据管理器 → Windows 凭据 → 普通凭据」里看到这三条，名称为 `Sparrow.Sync`。
- **口令不对会被明确指出来**：`meta.json` 里存了 `check`（口令派生密钥的 HMAC 校验值），所以口令与远端不一致时状态点变 `error` 并显示「同步口令与远端不一致」，**不会**假装"已同步"，也不会只给一句含糊的"解密失败"。
- **同步口令丢失不可恢复**：口令只用于派生 AES 密钥，服务端没有可用于还原数据的副本（`check` 只能校验、不能反推）。
- 同一同步目录（同一 `root_prefix`）下的**所有设备必须用同一个口令**，否则互相解不开对方的快照。
- **v1 不支持无感换钥**：要换口令，必须先清空远端 `{root_prefix}/`（`meta.json`、`devices/`、`blobs/` 全删），再在所有设备上填新口令重新开始（第一台连上的设备会用新 salt 重建 `meta.json`）。旧口令加密的远端历史数据在换钥后不可读，等于主动放弃。
- 想用脚本/CI 免写凭据管理器时，用环境变量（注意：环境变量对所有进程可见，属于降低安全性的做法）。

---

## 5. 安全模型

### 5.1 加密

| 项 | 值 |
|---|---|
| 对称加密 | AES-256-GCM（信封 `alg = "A256GCM"`），每个对象独立随机 12 字节 nonce |
| 密钥派生 | Argon2id，`m = 65536` KiB（64 MiB）、`t = 3`、`p = 1`、输出 32 字节，salt 16 字节随机 |
| KDF 参数位置 | `{root}/meta.json`，**明文**，含 `{v,kdf,m,t,p,salt,check}`；首次由某台设备创建，`If-None-Match: *` 保证并发首启只有一台成功。`check = hex(HMAC-SHA256(key, "sparrow-sync-meta-check-v1"))` 只用于校验口令是否与远端一致，**不含密钥本身**，也无法反推出密钥 |
| AAD | 一律为对象 key（例如 `sparrow/devices/<uuid>.json`），把密文绑定到对象路径 |
| JSON 信封 | `{"v":1,"alg":"A256GCM","nonce":"<b64>","ct":"<b64>"}` |
| 图片 blob 信封 | `b"SPRW1"` + 12 字节 nonce + ciphertext |

AAD 的作用：把 A 对象的密文挪到 B 对象上会解密失败，避免跨对象替换攻击。

### 5.2 传输

- **强制 HTTPS**；只有 `localhost` / `127.0.0.1` / `::1` 允许 `http://`（本地自建测试用）。
- **不跟随重定向**：任何 3xx 都按配置错误处理（`Config`，不可重试），并提示直接填写最终地址。原因：跟随重定向会把 301/302/303 上的 `PUT` / `PROPFIND` 改成 `GET` 并丢掉请求体（条件写语义失效），还会把 `Authorization` 带到新地址。
- **不允许协议降级**：非回环目标在 HTTP agent 层再设一道 `https_only`，重定向到 `http://` 同样会被拒。
- 回环地址还**绕过系统代理**，避免本地 mock 服务被代理劫持。
- 非回环目标会自动读取 Windows 系统代理（WinINET 设置），Clash/V2Ray 等无需额外配置。

### 5.3 远端可见 / 不可见

| 存储方能看到 | 存储方看不到 |
|---|---|
| 对象名（`root_prefix`、设备 UUID、图片/富文本的 `content_hash`） | 条目文本、链接原文、预览 |
| 对象大小、修改时间、ETag | 条目创建/使用时间戳 |
| `meta.json` 的 KDF 参数、salt 与 `check` 校验值 | 设备名（在密文快照里） |
| 请求频率（即同步活跃度） | 图片内容、富文本内容、数量与内容之间的关联 |

说明：图片与富文本副本都是**内容寻址**的，`blobs/{hash}.bin` 里的 hash 是明文字节（图片 / CF_HTML / RTF）的哈希。若攻击者已经猜到某份内容的确切字节，可以据此确认它是否在远端 —— 这是内容寻址的固有权衡。

### 5.4 日志脱敏

- 日志里只允许出现：操作名、后端类型、对象 key 的 SHA-256 前 8 位、**key 形状**（`devices` / `blobs` / `meta` / `root` / `other`）、字节数、HTTP 状态码、错误 kind。
- 禁止出现：URL 的 userinfo/query、`Authorization`、口令/密码/Secret Key、条目内容明文、图片文件全路径。
- 条目 hash 在日志里也只打前 8 位（如 `sync: skip sensitive item 1a2b3c4d`）。

### 5.5 敏感内容拦截（两道闸门 + 全量加密）

- 第一道（记录层，`exclusion_service`）：采集时命中内置规则就丢弃，不入库。覆盖范围：
  - PEM 私钥块（`-----BEGIN ... PRIVATE KEY-----`）
  - AWS Access Key ID（`AKIA`/`ASIA` + 16 位）
  - GitHub 令牌（`ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`、`github_pat_`）
  - Slack 令牌（`xox[baprs]-`）
  - `sk-` 前缀密钥（正文 ≥ 20 位，允许 `-`/`_`，覆盖 `sk-proj-`、`sk-ant-api03-`）
  - JWT（`eyJ...` 三段式）、`Bearer` 令牌、Google API Key（`AIza` + 35 位）
  - 中国大陆身份证号（18 位 + 校验位）、信用卡号（Luhn + 发卡行前缀 + 品牌长度）
  - 刻意**不**匹配 `password: xxx` 这类关键词，避免大量误报
- 第二道（同步层，`snapshot.rs`）：构建快照时对文本/链接**再查一次** `contains_sensitive`，命中就跳过（防止设置变更或历史数据绕过排除规则）。
- 兜底：即使两道都漏了，上传的仍是密文，服务端依旧看不到明文。
- 边界：**这两道闸门只覆盖记录层（是否入库/是否上传），不覆盖本地磁盘**。本地 `data/clipboard.db` 与 `data/images/` 是明文，磁盘保护依赖 BitLocker 等系统级加密。

---

## 6. 同步语义

### 6.1 远端布局

`{root_prefix}` 在路径里**只出现一次**：后端统一拼 `{base}/{root_prefix}/{key}`，命令层（如「测试连接」）只传相对 key。

```
{root_prefix}/meta.json                 # 明文：KDF 参数 + salt + check 校验值
{root_prefix}/devices/{deviceId}.json   # 密文：本设备全量快照（含墓碑）
{root_prefix}/blobs/{contentHash}.bin   # 密文：图片 / 富文本副本（HTML、RTF），内容寻址
{root_prefix}/.probe-{uuid}             # 测试连接的临时对象（用完即删）
```

- `meta.json` 内容为 `{v, kdf, m, t, p, salt, check}`：Argon2id 参数、salt，以及 `check`（口令派生密钥的 HMAC 校验值）。它**不含密钥**，也不含任何条目内容。
- 只有「测试连接」会创建 `.probe-{uuid}`：写完读回校验，随后**无论成功失败都会删除**（删除失败也不影响结论，只可能残留一个空对象）。历史上这里曾因为命令层自己拼了 `{root}`、后端又拼一次，导致真实路径变成 `{base}/{root}/{root}/.probe-*` → 父集合不存在，真实 WebDAV 直接 409；现已修正为只传 `.probe-{uuid}`。

### 6.2 快照与合并

- 每台设备上传的是**全量快照** `Snapshot { v, device_id, device_name, generated_at, records[] }`，不传增量。
- 每条记录 `SyncRecord { hash, kind, content, preview, created_at, updated_at, deleted_at, blob_size, html_blob, html_size, rtf_blob, rtf_size }`；`kind`：`0` 文本 / `1` 链接 / `2` 图片。快照格式版本 `v = 2`（v1 没有后四个字段；旧 peer 读到 v2 快照会忽略它们，只同步纯文本）。
- 拉取时按 ETag 判断：`devices/` 下 ETag 与本机记录相同的 peer 直接跳过，不 GET。
- **LWW**：同一 `content_hash` 冲突时取 `updated_at`（= 本地 `last_used_at`）较大的那条。
- **content_hash 去重**：本地已有同 hash 的条目时只更新 `last_used_at`，不重复插入。
- 上传去重：本机快照的 SHA-256 与上次相同则**完全跳过 PUT**（远端对象与 ETag 都不变）。
- 乐观并发：自己的快照用 `If-Match: 上次ETag`（首次用 `If-None-Match: *`），冲突时 HEAD 后重试一次，仍冲突则报「设备 ID 冲突」。
- 拉取后**只写本地库**，不写系统剪贴板 —— 因此不会出现"同步回来的内容又被采集一次"的回环。

### 6.3 同步方式、轮询与重试

`sync_mode` 决定"下一轮什么时候跑"：

| `sync_mode` | 定时器 | `clipboard-changed` / `item-deleted` | 唤醒来源 |
| --- | --- | --- | --- |
| `interval`（默认） | 每 `poll_interval_sec`（夹到 **2–300 秒**，默认 5 秒） | **立即唤醒**（400ms 去抖） | 定时器 + 事件 + 手动 + 保存设置 |
| `startup` | **不设定时器** | **不唤醒** | 只有手动「立即同步」、保存设置、切换模式 |

- **`startup`（仅启动时）**：进程启动后跑完一轮就静默，之后既不轮询也不响应本机复制/删除事件 —— 这是给"不想每 5 秒打扰存储服务"的场景准备的。
  - 手动「立即同步」在两种模式下都可用，是 `startup` 模式下唯一的按需拉取方式。
  - 删除墓碑（`item-deleted`）在 `startup` 模式下不会立刻上传：删除**立即**写入本地 `config/sync-tombstones.pending` 日志（一行一个 `{"hash":…,"deleted_at":…}`，纯本地写盘、不联网），等下一次启动或下一次手动同步的那一轮才合并进 `sync-state.json` 并让对端跟随删除。墓碑在删除瞬间就已落盘，**退出应用不会丢**：下次启动第一轮就会读回来，因此不会出现"本地已删、下次启动又从远端复活"。
  - 出错（凭据缺失 / 网络失败）后**不自动重试**，等下一次手动同步或重启。
- **`interval`**：`sync_now`（「立即同步」）、配置保存、删除钩子，以及**本机复制入库**（`clipboard-changed` / `item-deleted` 事件）都会立即唤醒一轮（带去抖，且同步自己写库触发的事件被忽略，避免回环）。
- 未启用：每 2 秒检查一次配置，无网络请求（两种模式一致）。
- 失败后下一轮间隔取 `max(轮询间隔, 30 秒)`，成功后恢复（仅 `interval` 模式会自动重试）。
- 重试：`Network` / `Server` / HTTP 429 最多 **5 次尝试**，退避 **500ms → 2s → 5s → 10s**（累计约 17.5s）；`Auth` / `Config` 类错误不重试（**重定向 3xx 与 http 协议降级都算 `Config`**，重试无意义）。
  - 为什么是 5 次：校园网/运营商 **DNS 污染是间歇的** —— 同一次同步里第一次解析失败、几秒后重试就成功很常见，多试几次能明显提高成功率。
  - **DNS 失败单独成文案**：`ErrorKind::Dns` 不再笼统报 `kind=Dns`，而是 `… 域名解析失败（DNS）: key=<8位tag> shape=<devices|blobs|meta>。校园网/DNS 污染环境下常见，请检查网络、DNS 或代理后重试`。
  - **peer 快照只试 2 次**：远端某个设备快照读不通时不为它等满 4 档退避，失败后该 peer 进入 **60 秒冷却**（`sync-state.json` 的 `peers.<id>.retry_after_ms`），期间不再请求；其它 peer 与本轮同步照常进行，状态栏会提示「N 个设备快照未拉取」。
- 超时：连接 5s；元数据请求 20s；blob 传输 20s；**DNS 解析 3s**（`TimeoutResolver`，见 9 节 DNS 行）。
- **连接不复用**：agent 关闭空闲连接池（`max_idle_connections(0)`），GET 另外显式发 `Connection: close`。复用被服务器/代理单方面关闭的 keep-alive 连接，正是 `response body closed before all bytes were read` 的典型来源。
- **GET 完整性校验**：响应带 `Content-Length` 时按实际读到的字节数核对，不一致报 `… 响应不完整: key=<tag> 期望 N 字节，实际 M 字节`（`Network`，可重试），绝不把半截内容当成功。
- **进度上报（`SyncStatus.phase`）**：`syncing` 期间后端按阶段上报进度点，设置面板与状态栏显示「同步中 · <阶段>」。阶段顺序：
  `连接存储` → `读取设备列表` → `合并设备快照` → `同步条目 i/n` → `上传附件` → `上传附件 i/n` → `上传快照`。
  - `同步条目 i/n` 的 `n` 是本轮要合并进来的条目数（只数 `Insert`，删除只是本地删行、不计入）；`上传附件 i/n` 的 `n` 是本轮还没传过的 blob 数（图片 + HTML/RTF 副本）。**首次全量同步大量图片时会长时间看到这两个文案**，序号会真的往前走，不是卡死。
  - `phase` 只含阶段名与计数，**不含内容、hash、文件名或路径**；状态离开 `syncing`（`idle` / `error`）时 `phase` 变回 `null`。
- **blob GET 只试 2 次（快速失败）**：单个图片/富文本 blob 读不通时最多 **2 次尝试**（每次受 20s 的 blob 超时约束，最坏约 40s），失败即跳过并计入 `skipped_images` / `skipped_formats`，状态栏提示「N 张图片未同步」，下一轮再试。所以**一个坏 blob 不会让一个周期无限停住**；反过来说，跳过是"这轮没同步上"，不是"永久放弃"。
- **blob PUT 也只试 2 次，且失败不中止整轮**：单个附件上传失败（2 次尝试后仍 5xx/超时/被代理截断）时，只跳过**这一个 blob** —— 它不会被记进 `sync-state.json` 的 `uploaded_blobs`（所以下一轮一定会重试），本轮计入 `upload_failed`，状态栏提示「N 个附件上传失败」。**快照照常上传**：文本/链接条目这一轮就能同步到其它设备，图片等下一轮补传。
  - 为什么要这样：以前 blob 上传失败会 `?` 中止整轮 → 周期返回 `error` → `last_sync_at` 永不更新，只要有一张传不上去的大图（实测 93 张图、最大 5.4MB），这台机器就"永远同步不了"，连纯文本也一起停摆。
  - 只有结构性错误（读库/遍历这类）才会让整轮失败；单个对象的传输失败一律降级成告警 + 下一轮重试。

### 6.4 图片

- 先传 blob 再传快照：`HEAD blobs/{hash}.bin` → 不存在则加密后 `PUT`（`If-None-Match: *`，409/412 视为"已存在"，正常跳过）。
- 接收方解密后写到 `data/images/{hash前16位}.png`（先写 `.tmp` 再 rename），再插库并带 `file_path`。
- 超过 `max_blob_mb` 或本地文件缺失/读不到的图片，快照里直接跳过。
- **单张图上传失败不阻断同步**：blob `PUT` 失败（2 次尝试后仍失败）只跳过该图，快照仍上传（文本/其它图片先同步过去），状态栏提示「N 个附件上传失败」，下一轮自动重试；连续多轮都失败再按 9 节排查带宽 / 代理 / `max_blob_mb`。
- **图片 blob 与富文本副本走的是同一条 GET 路径**（同一个 `Connection: close` + 长度校验 + 重试逻辑，见 6.3）：拉图片/HTML/RTF 时出现「响应不完整」或 `kind=Dns`，排查方式与设备快照完全一致 —— 都是网络侧问题，不需要单独处理。

### 6.5 富文本副本（HTML / RTF）

文本/链接条目在本地可能带 `html_path` / `rtf_path`（CF_HTML 包装与 RTF 流的**原始字节**，落在 `data/formats/`）。同步时：

- 快照里只带 `html_blob` / `html_size` / `rtf_blob` / `rtf_size`（blob = 原始字节的 sha256 hex，size = 字节数）；**绝不带本机绝对路径**。
- 远端对象是 `blobs/{html_blob}.bin` / `blobs/{rtf_blob}.bin`，键是**内容** hash，与 `record.hash` 无关：同一份 html 被多个条目共享时只上传一次。
- 上传前读文件并自校验 `sha256(bytes) == blob_hash`，不一致就跳过（下一轮快照重算会得到新 blob hash，自然重试）；读取后无条件 `PUT`（换口令必须覆盖旧密文，理由同图片）。
- 接收方解密 → 再校验 sha256 与 `*_size` → 写到 `data/formats/{record.hash 前 16 位}.html|.rtf`（先 `.tmp` 再 rename）。**不用 blob hash 命名**：两个条目共享同一份字节时，按 blob hash 命名会让它们指向同一个文件，删掉一个就把另一个的格式文件带走。
- 任一种格式的 blob 缺失/解密失败/超限 → 该列置 `NULL`、计入 `skipped_formats`，**纯文本照常插入**（降级为纯文本，不丢条目）。
- 发送端文件缺失 / 0 字节 / 超过 `max_blob_mb` 同样计入 `skipped_formats`（HTML 与 RTF 独立计数），并通过 `SyncStatus.warning` 提示，例如「3 张图片、2 个富文本未同步」。
- **不 sanitize、不解析**：RTF 的 `\bin` 段一旦被解析重构就会损坏；HTML 的净化在预览端（`get_rich_preview` + 前端 iframe `sandbox=""`）已经做过。
- 威胁模型：所有设备共享同一口令，视为**同一信任域**；远端只有密文，故不在传输链路里做净化（净化会破坏无损往返）。信任域内某台设备若被攻陷，它本来就能读到明文，净化与否不影响结论。
- 删除条目时同时删本地 `html_path` / `rtf_path` 指向的文件（与图片同源的 canonicalize + `starts_with` 守卫，目录外一律不碰）。

### 6.6 删除

- 只有**显式删除**（删除单条、清空历史）才产生墓碑：`content_hash` + `deleted_at`，随下一次快照上传，其它设备据此删除本地对应条目与图片文件。
- **retention / 条数上限等自动清理不产生墓碑，也不传播**。B 上被 retention 清掉的条目，不会导致 A 上同一条被删；同理 B 手动收藏的条目不会因为 A 的自动清理而消失。
- 墓碑保留 **60 天**（`TOMBSTONE_TTL_MS`），超期裁剪；`uploaded_blobs` 记录上限 **2000** 条（丢了只是多发一次 HEAD）。
- 若某条目在墓碑之后又被使用（`updated_at > deleted_at`），视为"复活"，不再被墓碑压制。

### 6.7 不同步的内容

- 文件路径条目（`type = 3`）。
- 收藏分组关系（收藏状态本身是本地概念，不传播）。

（富文本 HTML / RTF 副本自快照 `v = 2` 起同步，见 6.5。）

---

## 7. 首次配置步骤

两台设备都要做，且**口令必须完全一致**。

1. 打开「设置 → 云同步」。
2. 选择后端（WebDAV / S3 兼容），填写地址类字段（见第 2 节示例）。
3. 填「设备名」（建议 `DESKTOP-A` / `LAPTOP-B` 这类可辨认的名字）。
4. 填「口令」（同步口令），点保存 —— 写入凭据管理器 `Sparrow.Sync` / `passphrase`。两台设备用同一串。
5. 填 WebDAV 密码 或 S3 Secret Key（失焦即保存）。
6. 点「测试连接」。成功时返回 `连接成功 · N ms`（内部会 MKCOL 建目录、`PUT` 一个 256 KB 伪随机探测对象、逐字节读回校验、再 `DELETE` 掉）。探测对象的 key 是 **`{root_prefix}/.probe-{uuid}`** —— 命令层只传相对 key `.probe-{uuid}`，`{root_prefix}` 由后端拼；无论成功失败都会尝试删除，远端不会留下垃圾对象。用 256 KB 而不是 2 字节，是为了暴露“控制面通、数据面被代理/MTU 截断”的网络（真实故障里出现过测试连接通过、同步大 blob 被截断）。
7. 选「同步方式」：默认「按间隔轮询」（实时性最好，但会周期性请求存储服务）；若只想每次启动同步一次、平时完全静默，改选「**仅启动时**」——此时不再显示轮询间隔，复制/删除也不会触发同步，只能靠「立即同步」手动拉取。
8. 打开「启用云同步」。状态点应从 `disabled` 变为 `idle`，随后（`interval` 模式）按轮询间隔周期性变成 `syncing`。
9. 确认 `device_id` 已生成：打开 `<程序目录>\config\sync-state.json`，应能看到非空 `device_id`（UUID v4）。两台设备的 `device_id` **必须不同**（相同说明两个便携目录共用了同一份 `config/`）。

---

## 8. 双设备手动验收步骤

### 8.0 准备

- [ ] 两台机器（或同一台机器上的两个便携目录），各自有独立的 `config/` 与 `data/`：
      ```
      D:\portable-A\config\settings.json
      D:\portable-A\config\sync-state.json
      D:\portable-A\data\clipboard.db
      D:\portable-B\config\...
      D:\portable-B\data\...
      ```
- [ ] 两个实例指向**同一个** WebDAV 目录 / 同一个 S3 bucket + 同一 `root_prefix`。
- [ ] 两边口令一致，`device_name` 分别为 `DESKTOP-A` / `LAPTOP-B`，`poll_interval_sec` 都设为 `5`。
- [ ] 两边都点「测试连接」成功，再「启用云同步」。
- [ ] 两边 `config/sync-state.json` 的 `device_id` 不同。

> 时延说明：本机复制/删除会立刻唤醒一轮同步（`clipboard-changed` / `item-deleted`），所以 A 复制后**无需**再点「立即同步」，A 会马上上传；对端 B 仍靠自己的轮询，因此"从复制到对端出现"最坏约为**一个**轮询间隔（默认 5 秒，不再是 2×）。若要更稳的复现，仍可在 A 点一次「立即同步」再掐表等 B。

### 8.1 普通文本

- [ ] A 复制一段普通文本（例如 `hello cloud sync 001`）。
- [ ] A 应在约 1 秒内自动进入 `syncing` 再回到 `idle`（`clipboard-changed` 唤醒，不需要手点）；点「立即同步」同样可以。
- [ ] B 在 ≤ 轮询间隔 + 1s 内出现同一条内容，内容完全一致。
- [ ] 反向验证一次（B 复制 → A 出现）。

### 8.2 敏感内容拦截

- [ ] 生成一个**假的** `sk-` 前缀测试串（不要用真实密钥）。示例（拼接构造，避免任何真实形态）：
      ```powershell
      # 仅用于测试拦截：sk- 之后是 24 个占位字符，命中 RE_SK_KEY
      $fake = "sk-" + ("test0" * 4) + "0000"
      Set-Clipboard -Value $fake
      ```
      等价 bash 写法：
      ```bash
      printf 'sk-%s0000' "$(printf 'test0%.0s' 1 2 3 4)" | clip.exe
      ```
- [ ] 记录远端 `devices/` 下 A 的对象的 **大小与 ETag**（改动前）。
- [ ] A 复制上面的假密钥，点「立即同步」，等状态回到 `idle`。
- [ ] A 的列表里**没有**这条记录（采集层已丢弃；若采集层关闭了敏感识别，同步层也会跳过）。
- [ ] B 上**不出现**这条内容（等 ≥ 一个轮询间隔再确认）。
- [ ] 远端 `devices/` 下 A 的对象**大小与 ETag 均未变化**（快照摘要相同 → 完全没有 PUT）。
- [ ] 应用日志中若有 `sync: skip sensitive item <8位>`，属于预期。

### 8.3 图片

- [ ] A 截图或复制一张图片（PNG）。
- [ ] A 点「立即同步」，等 `idle`；确认远端出现 `blobs/{hash}.bin`。
- [ ] B 在 ≤ 一个轮询间隔 + 2s 内出现一条 `[图片]` 条目，可预览、可粘贴回剪贴板。
- [ ] B 的 `data/images/` 下出现对应 `.png` 文件。
- [ ] （可选）把 `max_blob_mb` 改成 `0` 并重启，确认大图被跳过且不影响文本同步。

### 8.4 删除与 retention

- [ ] A 删除 8.1 那条文本 → 点「立即同步」。
- [ ] B 在 ≤ 一个轮询间隔 + 2s 内跟随删除该条（含图片文件）。
- [ ] 在 B 上把一条内容**收藏**；在 A 上把 `max_item_count` 调到很小（如 `3`）触发清理任务，让 A 把同一条自动清掉。
- [ ] B 上那条被收藏的条目**仍然存在**（retention 不产生墓碑、不传播）。

### 8.5 断网恢复

- [ ] 断开 A 的网络（禁用网卡 / 关代理），保持 **2 分钟**，期间在 A 上连续复制 5 条不同内容。
- [ ] A 的状态点为 `error`，`last_error` 显示网络类错误；本地功能不受影响（复制、粘贴、搜索正常）。
- [ ] 恢复网络，等 A 状态回到 `idle`，再等 B 一轮轮询。
- [ ] B 上补齐这 5 条，且**每条只出现一次**（无重复）。
- [ ] A 上这 5 条也**各只出现一次**（拉取只写库、不写剪贴板，因此没有回环新增）。

### 8.6 并发写入

- [ ] 在 A、B 上几乎同时各复制一条不同内容（尽量在同一秒内）。
- [ ] 两边各自点一次「立即同步」，等两边都回到 `idle`。
- [ ] 最终 A、B 上都有这**两条**内容；两边内容集合一致，没有互相覆盖或丢失。

### 8.7 远端与日志检查

- [ ] 下载远端 `devices/*.json`，用文本编辑器打开，应为 `{"v":1,"alg":"A256GCM","nonce":"...","ct":"..."}` 形态的 base64 密文；看不到任何中文/英文原文。
- [ ] 在下载目录里搜明文：
      ```bash
      grep -rn "hello cloud sync" .        # 应无输出
      grep -rniE "sk-[A-Za-z0-9_-]{19,}" . # 应无输出
      ```
- [ ] 下载任一 `blobs/*.bin`，文件头应为 `SPRW1`，之后是二进制密文（不是 PNG 魔数 `\x89PNG`）。
- [ ] 检查应用日志（若有落盘日志）：
      ```bash
      grep -niE "password|passphrase|secret|authorization|bearer" <日志文件>   # 应无输出
      ```
      只应看到 `sync: <操作> 失败: key=<8位hex> kind=<...>` 这类行。
- [ ] `config/settings.json` 里搜不到口令/密码/Secret Key：
      ```bash
      grep -niE "passphrase|password|secret_key" <程序目录>/config/settings.json   # 应无输出
      ```

---

## 9. 故障排查

| 现象 | 常见原因 | 处理 |
|---|---|---|
| `401` / `403`，状态 `error` | WebDAV 密码错、用了登录密码而非应用密码；S3 Access Key / Secret 不匹配或无权限 | 重新保存凭证；坚果云/Nextcloud 用「应用密码」；确认 bucket 策略允许 PutObject/GetObject/ListBucket |
| `404` | WebDAV URL 少写层级、集合不存在；S3 bucket 名错或 region 错导致域名不存在 | 先用「测试连接」；WebDAV 的父集合必须已存在（应用只会 MKCOL 最后几级）；确认 `root_prefix` 没有重复 |
| `412` / 提示「设备 ID 冲突」 | 同一个 `device_id` 被两个实例同时使用（复制了整个 `config/` 目录、或两台机器共用同一便携目录） | 关掉其中一个实例，删除冲突方的 `config/sync-state.json`（会生成新 `device_id`，需重新同步一次全量） |
| `409` | `meta.json` 或 blob 已存在（正常并发路径，多数会被自动降级处理）；也可能是 WebDAV 父目录缺失 | 若是持续报错，检查父集合权限 |
| 证书错误（`certificate` / `tls`） | 自签证书、企业中间人代理、系统时间错误 | 换受信任证书；或给系统装上根证书；校时 |
| 连接超时 / 一直 `error` | 需要代理但未配置；DNS 或防火墙拦截 | 应用会读 Windows 系统代理设置，先在浏览器里确认能访问；`localhost` 目标不走代理 |
| **`kind=Dns` /「域名解析失败（DNS）」**（校园网最常见） | **DNS 污染/间歇解析失败**：校园网、运营商对境外域名的解析被投毒或限流，浏览器能打开（走了代理/DoH）但应用直连解析不出来。同一域名时好时坏。实测某校园网：对 `app.koofr.net` 的 **UDP/53 查询被静默丢包**（校园 DNS 与 223.5.5.5 都超时），但 **TCP/53、DoH、443/TLS 全部正常**——说明是 DNS 层拦截，不是域名/IP 被封 | ① **hosts 钉 IP（最快）**：管理员编辑 `C:\Windows\System32\drivers\etc\hosts`，加 `167.235.4.177 app.koofr.net`（备用 `5.9.21.137` / `116.202.160.164`）；证书 `*.koofr.net` 仍然有效。② 开系统代理（Clash / V2Ray，打开「系统代理」或 TUN；应用只读 `ProxyEnable=1` 的 WinINET 设置）。③ Win11 网卡 DNS 设为 `223.5.5.5` / `1.1.1.1` **并启用「通过 HTTPS 的 DNS」**（只改 DNS 地址、仍走 UDP/53 时实测无效）。④ 换热点验证。⑤ 客户端会自动重试 5 次，但网络侧拦截无法根治。⑥ **DNS 解析超过 3 秒会以 `域名解析失败/超时` 失败并重试，不会无限卡在同步中；根治仍需 hosts/代理/DoH** |
| 「`… 响应不完整 …`」/「`response body closed before all bytes were read`」（GET 中途断开） | 大文件（图片/富文本 blob）或代理在传输中途断开连接；MTU/分片被中间设备丢弃；服务器提前关闭 keep-alive | 客户端已加 `Connection: close` + 不复用连接 + 长度校验 + 重试 5 次；若持续出现：检查代理（关闭/换节点）、把 MTU 调到 1400 左右、换网络；单个 peer 快照读不通只会跳过该 peer（60s 冷却），不会阻断整轮 |
| 状态栏「N 个设备快照未拉取」 | 远端 `devices/` 里有个读不通的对象（常见：第三台旧设备留下的快照、写了一半的对象） | 该 peer 已进入 60s 冷却，其它设备与本机同步不受影响；确认那台设备是否还在用，不再用就删掉远端 `{root_prefix}/devices/<id>.json` |
| 状态栏「N 个附件上传失败」 | 某些图片/富文本 blob 上传失败（2 次尝试后仍 5xx / 超时 / 传输中断）：带宽不足、代理截断大对象、或单张图接近 `max_blob_mb` | **不会阻断文本同步**：失败的 blob 不记入 `uploaded_blobs`，下一轮自动重试，文本/其它条目照常同步。持续出现时按 6.3 排查网络与代理，或把 `max_blob_mb` 调小（超限的图会被直接跳过并计入 `skipped_images`）；日志里对应 `sync: upload blob <hash前8位> failed kind=…` |
| 状态长时间停在「同步中 · 同步条目 i/n」/「上传附件 i/n」 | **首次全量同步大量图片**（几百张时要下载/上传几百个 blob）；或某台设备/某个 blob 读得很慢 | 进度点的序号会持续推进，属正常；单个 blob 最多 **20s × 2 次**失败后就跳过并计入告警（下载失败「N 张图片未同步」、上传失败「N 个附件上传失败」），**不会无限停在一个周期**。若长时间停在同一序号：检查带宽/代理，或把 `max_blob_mb` 调小；坏对象下一轮会再试 |
| S3 `SignatureDoesNotMatch` | **系统时钟偏差**、`region` 与服务商不一致、endpoint 写成了 IP 却关了 path-style | 同步系统时间（NTP）；核对 region（R2 用 `auto`）；自定义域名/IP 开 `s3_path_style` |
| `RequestTimeTooSkewed` | 同上，时钟偏差超过 15 分钟 | 校时 |
| `429` / `503`，反复重试 | 轮询过频被服务商限流（尤其 2 秒间隔 + 大快照） | 把 `poll_interval_sec` 调大到 15–60；缩小条目量；S3 用户注意 LIST 请求计费 |
| 状态一直 `disabled` | `enabled` 未开 | 打开「启用云同步」，保存后立即生效 |
| `需要设置同步口令` | 凭据里没有 `passphrase` | 在设置面板填口令并保存（或设 `SPARROW_SYNC_PASSPHRASE`） |
| 「同步口令与远端不一致」 | 口令填错；或远端 `meta.json` 是另一套口令（换过口令 / 换过目录 / 别人先占了同一个 `root_prefix`） | 核对两边口令；确认无误后清空远端 `{root_prefix}/`，用统一口令重建（v1 不支持无感换钥） |
| 「解密失败」 | 远端 `meta.json` 被替换过，或口令与生成 `meta.json` 时的口令不同 | 同上：统一口令，必要时清空远端目录重来 |
| `WebDAV 地址不要包含用户名或密码…` | URL 里内嵌了 userinfo（`https://user:pass@host/`） | 从 URL 去掉 userinfo；用户名填「用户名」，密码填「密码」 |
| 「…遇到重定向」/「…被重定向」 | 服务器对集合路径返回 3xx（尾斜杠跳转、HTTP→HTTPS 跳转） | 同步**不跟随重定向**：直接把最终地址填进 `webdav_url`（这类错误是 `Config`，重试无用） |
| B 一直不出现新内容 | B 的 `root_prefix` / bucket / 目录与 A 不同；B 未启用；B 处于 `error` | 对照两边 `settings.json` 的 `sync` 段；看 B 的 `last_error` |

---

## 10. 已知限制

1. **全量快照带宽**：每轮有变化就上传整份快照。500 条约 100–200 KB/次；条目很多或轮询很密时，上传流量与 LIST 请求量都随之上升。
2. **远端旧 blob 不清理**：图片按内容寻址，可能被其它条目引用，v1 不主动删除 `blobs/` 下的对象。删除条目只会删本地图片与远端快照里的记录。
3. **口令不可找回**：口令丢失 = 远端数据永久不可读，没有恢复通道；换口令等于换了一套密钥，需要清空远端目录重新开始。
4. **同 `deviceId` 双开**：同一份 `config/` 被两个实例使用会互相顶掉（`If-Match` 412），表现为「设备 ID 冲突」。不要共享 `config/` 目录。
5. **移动端无后台轮询**：桌面端才有常驻轮询循环；移动端/后台受限场景只能手动触发「立即同步」。
6. **不做版本历史**：只有当前状态，删除会传播，没有"从某台设备恢复已删条目"的能力（墓碑 60 天内可压制复活）。
7. **收藏关系不同步**：收藏是本地状态，不同设备各自维护。
8. **本地明文**：`data/clipboard.db` 与 `data/images/` 未加密，磁盘级保护依赖系统加密。
9. **图片哈希可见**：内容寻址意味着远端能看到图片内容的哈希，见 5.3 的说明。

---

## 11. 验收对照表

| # | 验收标准 | 如何验证 | 结果 |
|---|---|---|---|
| 1 | A 复制普通文本，B 在轮询间隔内出现 | 8.1：A 复制（本机 `clipboard-changed` 立即唤醒上传）→ 掐表看 B 出现时间（≤ 间隔 + 1s）；即使不点「立即同步」最坏也只约一个间隔 | ☐ 通过 / ☐ 不通过 |
| 2 | 敏感内容被拦截后不进同步队列 | 8.2：A 复制拼接构造的假 `sk-` 串 → A 无该记录、B 无该内容、远端 `devices/` 对象大小与 ETag 不变 | ☐ 通过 / ☐ 不通过 |
| 3 | 远端只存密文、日志无明文 | 8.7：`devices/*.json` 为 base64 信封、`blobs/*.bin` 以 `SPRW1` 开头、`grep` 不到明文与凭证、`settings.json` 无口令 | ☐ 通过 / ☐ 不通过 |
| 4 | 多设备同时写不互相覆盖 | 8.6：两边同秒各复制一条 → 两边最终都有两条；每设备独立对象 + `If-Match` 保证不互相顶掉 | ☐ 通过 / ☐ 不通过 |
| 5 | 断网恢复不重复、不回环 | 8.5：断网 2 分钟期间 A 复制 5 条 → 恢复后 B 补齐且各一条，A 也各一条（拉取不写剪贴板） | ☐ 通过 / ☐ 不通过 |
| 6 | 不破坏本地功能 | 采集/粘贴/搜索/图片预览/retention 均正常；`cargo test --lib`、`vitest`、`tsc` 全绿 | ☐ 通过 / ☐ 不通过 |

---

## 附录：实现位置（排查代码时用）

| 关注点 | 文件 |
|---|---|
| 配置字段与默认值、快照/记录结构 | `src-tauri/src/models/sync.rs` |
| 凭证 service 名、环境变量名、优先级 | `src-tauri/src/services/credentials.rs` |
| 加密、KDF、信封、AAD | `src-tauri/src/services/sync/crypto.rs` |
| 同步周期、轮询、重试、blob 收发、设备冲突 | `src-tauri/src/services/sync/service.rs` |
| 后端抽象、错误映射、超时、代理、https 校验 | `src-tauri/src/services/sync/backend.rs`、`src-tauri/src/services/sync/mod.rs` |
| WebDAV 适配（MKCOL/PROPFIND/条件写） | `src-tauri/src/services/sync/webdav.rs` |
| S3 适配（SigV4/ListObjectsV2/寻址） | `src-tauri/src/services/sync/s3.rs`、`src-tauri/src/services/sync/sigv4.rs` |
| 快照构建与敏感内容二次过滤 | `src-tauri/src/services/sync/snapshot.rs` |
| 合并规则（LWW/墓碑/去重） | `src-tauri/src/services/sync/merge.rs` |
| 本地状态（deviceId、peer ETag、墓碑、裁剪） | `src-tauri/src/services/sync/state.rs` |
| 前端命令（状态/立即同步/写凭证/测试连接） | `src-tauri/src/commands/sync.rs` |
| 设置面板暴露的字段 | `src/components/SyncSection.tsx` |
| 敏感内容识别范围 | `src-tauri/src/services/exclusion_service.rs` |
| 实施计划与设计取舍 | `docs/superpowers/plans/2026-10-24-cloud-sync.md` |
