# 发布新版本（维护者指南）

面向维护者：从改版本号到 GitHub Release 与更新通道的完整流程。当前产品为 **Sparrow / 剪雀 1.1.1**。

> 约定：安装包 `sparrow-setup.exe`、主程序 `sparrow.exe`；GitHub 仓库已改名为 `https://github.com/QikiiL/Sparrow`（旧的 `QikiiL/Clipboard` 地址仅作为 0.x 更新兼容入口，GitHub 会自动重定向到新仓库）。

## 1. 改版本号（6 处必须一致）

| 文件 | 位置 |
| --- | --- |
| `package.json` | 顶层 `version` |
| `package-lock.json` | 顶层 `version` 与 `packages[""]` 下的 `version`（两处，`npm install` 会自动同步） |
| `src-tauri/tauri.conf.json` | 顶层 `version` |
| `src-tauri/Cargo.toml` | `[package]` 下的 `version` |
| `src-tauri/Cargo.lock` | 应用自身包块（`name = "sparrow"`）下的 `version` |
| `version.json` | `version` + `notes` + `github`（Release 地址） |

安装包文件名与更新检查都读这些值。漏改 `Cargo.lock` 会让锁文件与 `Cargo.toml` 不一致，构建时被 cargo 覆写或报错；`package-lock.json` 的根 `version` 与 `packages[""]` 下的 `version` 两处通常由 `npm install` 自动同步，手改时别漏。

`version.json` 的中文更新说明请用 **Python 以 UTF-8 写入**，不要用 PowerShell 5.1 的 `Set-Content`（会写成 UTF-16 / GBK，客户端读到乱码）。

## 2. 打包

1. **结束占用 target 的进程**：先停 `sms-helper.exe` / `sms-capture-helper.exe`，否则链接期报 `LNK1104`（文件被占用）。**不要杀正在运行的主程序**——它持有剪贴板监听，且热键接管会短暂失效。
2. **手动清空 `dist/`**：`vite.config` 里 `emptyOutDir` 刻意设为 `false`，残留旧文件会被打进包。本机 safe-delete 垫片会静默拦截 `Remove-Item` 的批量删除，改用：
   ```powershell
   [System.IO.Directory]::Delete($p, $true)   # 删完要复核目录是否真的没了
   ```
3. **确认资源在位**：`src-tauri/identity/` 下的 MSIX 与 `.cer`，以及 `sms-helper.exe`。`beforeBuildCommand` 会把它们拷到 `src-tauri/` 根目录并现场编译 `sms-capture-helper`；这三份文件同时是 NSIS 的资源清单（`tauri.conf.json` 的 `bundle.resources`），缺任何一个都会打包失败。
4. **构建**：`npm run tauri build`（或 `build-release.bat`）。

## 3. 产物与上传名

- 安装包（NSIS，中文向导）：`src-tauri/target/release/bundle/nsis/Sparrow_<版本>_x64-setup.exe` → 复制成仓库根的 `sparrow-setup.exe`。
- 便携版主程序：`src-tauri/target/release/sparrow.exe` → 复制成仓库根的 `sparrow.exe`。

复制后核对字节数与时间戳确属本次构建，避免把上一版传上去。仓库根的 `*.exe` 被 `.gitignore` 忽略，不会误提交。

> 没装 [NSIS](https://nsis.sourceforge.io/) 时，`tauri build` 会在最后的打包步骤失败，但 `target/release/sparrow.exe` 已生成——直接拷出来放进空文件夹就是便携版。

## 4. 打标签并推送

用附注标签，消息形如「vX.Y.Z: 一句话摘要」。中文写进文件再用 `-F` 传入，避免 PowerShell 参数编码问题：

```bash
git tag -a vX.Y.Z -F notes.txt
git push origin master
git push origin vX.Y.Z
```

## 5. 发布

1. 在 GitHub Releases 选该标签新建 Release，上传 `sparrow-setup.exe` 与 `sparrow.exe` 两个固定名资产。
2. 蓝奏云是**长期不变的目录分享**：<https://wwbwp.lanzoul.com/b00ybd8mej>（密码 `bdlx`）。只需把新安装包传进该目录，地址无需改动。
3. Release 正文建议结构：`## 更新内容` → `### 新功能 / 改进 / 修复 / 升级说明` → `## 下载`。

## 6. 验证更新通道

- 主地址：`https://raw.githubusercontent.com/QikiiL/Sparrow/master/version.json`
- 兜底：`https://cdn.jsdelivr.net/gh/QikiiL/Sparrow@master/version.json`（约 12 小时缓存；校验时用 commit 定版 URL `…/gh/QikiiL/Sparrow@<commit>/version.json` 绕过缓存才看得到最新内容）

确认 Release 非草稿、两个资产大小与本地一致后，把 `version.json` 推到 `master`。已安装用户启动应用即收到更新提醒。

## 7. 1.0.0 改名相关的额外检查

1.0.0 把产品名从 Clipboard 改为 Sparrow / 剪雀，发版前后请额外确认：

- **兼容身份不变**：自启动任务名已从 `ClipboardManagerAutostart` 改为 `SparrowAutostart`；除此之外的 `identifier`（`com.lyz.clipboard-manager-tauri`）、MSIX 包名与两个 AUMID 不要动，否则老用户配置找不到、短信捕获链路断。
- **升级路径**：0.x 的安装目录是用户自选的（常见 `%LOCALAPPDATA%\clipboard`，也可能是 `D:\clipboard`），安装器读旧卸载项 `…\Uninstall\clipboard` 的 `InstallLocation` 并**原地升级**（目录页/重装页跳过、装完自动启动）；只有用户手动改到别的目录才把旧 `config`/`data` 复制过去且不覆盖已有文件。安装后清理旧卸载项、`Software\lyz\clipboard`、`clipboard*.lnk` 快捷方式与旧 `clipboard-manager-tauri.exe`。Release 说明里要写清「旧版会被原地替换，历史与设置自动保留」。
- **回归清单**：冷启动 / 覆盖安装（0.x → 1.0.0）/ 卸载后重装 / 短信验证码捕获 / 开机自启 / 检查更新 / 便携模式（exe 换目录）。
- **构建脚本**：`build-release.bat` 里 NSIS 产物的 glob（`Sparrow_*_x64-setup.exe`）与复制目标名（`sparrow-setup.exe`）要与 `productName` 同步。
- **安装器脚本**：`src-tauri/nsis-hooks.nsh` 里 `taskkill /F /IM "sparrow.exe"`、卸载备份目录名与中文提示文案需与新产品名一致。

## 8. 常见坑速查

| 现象 | 原因 / 处理 |
| --- | --- |
| 链接期 `LNK1104` | `target` 下的 exe 被占用：先停 `sms-helper.exe` / `sms-capture-helper.exe` |
| 安装包里混入旧前端 | `dist/` 未清（`emptyOutDir: false`），按第 2 步用 .NET API 删并复核 |
| 更新说明乱码 | 用 PowerShell 5.1 写了中文 JSON，改用 Python UTF-8 写入 |
| 构建脚本删不掉文件 | 本机 safe-delete 垫片拦截批量删除，改用 `[System.IO.Directory]::Delete` |
| 客户端检查不到新版本 | `version.json` 未推到 `master`，或 jsdelivr 缓存未过期 |
| 老用户升级后「历史没了」 | 安装器没识别到旧版：检查 `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\clipboard` 的 `InstallLocation` 是否指向旧目录（原地升级不复制数据，路径没对上就会装成新软件） |

## 附：版本编号沿革（2026-10 重编号）

产品在 1.0.0 之前叫 Clipboard，1.0.0 起改名 Sparrow / 剪雀。历史版本号曾整体重编号一次，对照如下：

| 旧编号 | 新编号 | 说明 |
| --- | --- | --- |
| v0.1.0 – v0.4.4 | 不变 | 早期 Clipboard 版本 |
| v1.0.0 – v1.4.3 | v0.5.0 – v0.9.3 | 更名前最后一程，重编号后落在 1.0 以下 |
| v2.0.0 | v1.0.0 | 改名为 Sparrow 的起点 |
| v2.1.0 | v1.1.0 | 云同步 |
| v2.1.1 | v1.1.1 | 粘贴目标与连续粘贴修复（当前） |

git tag 与 GitHub Release 已按上表重命名；**commit message 里的旧版本号属于历史记录，不会改**（例如 `chore: release 2.1.1`），看到时按上表换算即可。
