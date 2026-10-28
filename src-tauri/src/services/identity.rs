//! 稀疏 MSIX 包身份(松散外部位置打包,sparse / loosely-provisioned package)。
//!
//! 给非打包的 Win32 桌面应用补上 MSIX 包身份,从而能订阅
//! `UserNotificationListener::NotificationChanged` —— 无包身份时该订阅必报
//! `0x80070490`(ERROR_NOT_FOUND),轮询接口则不受影响(现状降级为 1s 轮询)。
//!
//! 什么是稀疏包:一个只声明「身份」、几乎不含 payload 的 MSIX,带
//! `uap10:AllowExternalContent` 与 `uap10:RuntimeBehavior="win32App"`。
//! 它告诉系统「应用主体(exe 及资源)放在外部目录」,但 exe 本身不进包、
//! 不被虚拟化。注册后,只要进程以这个身份启动,就拥有包身份。
//!
//! 为什么能解决 0x80070490:`NotificationChanged` 是 UWP 风格的「带身份事件」,
//! 系统要求订阅者具备 MSIX 包身份。注册稀疏包给进程补上身份后订阅即可成功。
//!
//! ⚠️ 进程身份在创建时确定:注册发生在本进程启动之后,所以对「当前已在运行
//! 的进程」订阅不一定立刻生效 —— 常见情况是要重启应用后事件驱动才生效。
//!
//! 卸载(一句带过):`Remove-AppxPackage` 同名包,或用
//! `PackageManager::RemovePackageAsync` 按包全名卸载。
//!
//! 本模块的所有失败都只返回 `Err(String)`,绝不 panic;自身逻辑开销极小,
//! 最多重试一次注册。注意:注册本身是系统部署操作,耗时不由本模块控制
//! (实测首次注册遇到不受信任证书时,系统做证书链校验可能耗时数分钟),
//! 只保证失败一律返回 Err、由调用方降级。
//!
//! ⚠️ 本模块被 examples/sms-capture-helper.rs 通过 `#[path]` 引入独立编译,
//! 因此**不得**引用 `crate::` 下的任何项(含 lib.rs 的 exe_dir),依赖只限
//! windows/std —— 看似重复的本地辅助函数是有意保留的。

use std::path::{Path, PathBuf};
use std::time::Duration;

use windows::core::HSTRING;
use windows::Foundation::Uri;
use windows::Management::Deployment::{AddPackageOptions, PackageManager};

/// 清单里声明的身份(与 AppxManifest.xml 保持一致)
const PACKAGE_NAME: &str = "ClipboardManagerIdentity";
const PACKAGE_PUBLISHER: &str = "CN=ClipboardManager";

/// 本应用的 AUMID = 包族名(PFN)!应用 Id。PFN 由 Name+Publisher 确定性
/// 哈希而来,只要清单里 Name/Publisher 不变就不会变(实测值,改动清单后
/// 需重新确认)。Shell 激活(AUMID)是稀疏包身份授予的唯一途径:直接
/// 启动 exe 的进程没有身份,必须经 ActivationManager 拉起。
pub const APP_AUMID: &str = "ClipboardManagerIdentity_z0fnhbkv2vcxr!ClipboardManagerApp";

/// sms-helper.exe 的 AUMID(包族名!Application Id,与 AppxManifest.xml 里
/// Id="SmsHelper" 的条目对应)。主程序激活 helper 用:helper 是 asInvoker
/// 的轻量进程,Shell 以 AUMID 激活后自带稀疏包身份,能订阅 NotificationChanged
pub const HELPER_AUMID: &str = "ClipboardManagerIdentity_z0fnhbkv2vcxr!SmsHelper";

/// 当前进程是否具备包身份(进程身份在创建时确定,注册对运行中的进程无效)
#[allow(dead_code)]
pub fn has_identity() -> bool {
    windows::ApplicationModel::AppInfo::Current().is_ok()
}

/// 以包 AUMID 经 Shell 激活重启应用:新进程出生即带稀疏包身份,
/// 从而能订阅 NotificationChanged。只发激活请求,不等待新进程;
/// 调用方负责随后退出当前进程(app_handle.exit(0))。
#[allow(dead_code)]
pub fn relaunch_self_via_identity() -> Result<(), String> {
    unsafe {
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Shell::{
            ApplicationActivationManager, IApplicationActivationManager, ACTIVATEOPTIONS,
        };
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mgr: IApplicationActivationManager = CoCreateInstance(
            &ApplicationActivationManager,
            None,
            CLSCTX_LOCAL_SERVER,
        )
        .map_err(|e| format!("CoCreateInstance(ApplicationActivationManager) 失败: {e}"))?;
        mgr.ActivateApplication(
            &HSTRING::from(APP_AUMID),
            &HSTRING::from(""),
            ACTIVATEOPTIONS(0),
        )
        .map_err(|e| format!("ActivateApplication 失败: {e}"))?;
        Ok(())
    }
}

/// 以 helper 的包 AUMID 经 Shell 激活拉起 sms-helper.exe:helper 是 asInvoker
/// 的轻量进程,Shell 激活后出生即带稀疏包身份,能订阅 NotificationChanged
/// 并直接写剪贴板(提权的主进程做不到,见模块头注释)。
/// 返回激活的进程 PID,调用方用于看门狗监控。
/// 前提:稀疏包已注册且清单里含 Id="SmsHelper" 条目(调用方先 ensure)。
#[allow(dead_code)]
pub fn activate_helper() -> Result<u32, String> {
    // 前置检查:helper exe 必须与主程序同目录。缺失时(安装异常/被安全软件
    // 清理)绝不能发起 Shell 激活 —— 激活失败会弹「Windows 找不到文件」的
    // 系统错误对话框(0.2.3 在用户机上实测),且无法从代码侧抑制。
    // 缺失时静默返回 Err,调用方降级为 1s 轮询。
    let helper = exe_dir().ok_or_else(|| "取不到当前 exe 所在目录".to_string())?;
    let helper = helper.join("sms-helper.exe");
    if !helper.is_file() {
        return Err(
            "sms-helper.exe 不存在于安装目录,跳过激活(短信捕获降级为 1s 轮询)".to_string(),
        );
    }

    unsafe {
        use windows::Win32::System::Com::{
            CoCreateInstance, CoInitializeEx, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Shell::{
            ApplicationActivationManager, IApplicationActivationManager, ACTIVATEOPTIONS,
        };
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let mgr: IApplicationActivationManager = CoCreateInstance(
            &ApplicationActivationManager,
            None,
            CLSCTX_LOCAL_SERVER,
        )
        .map_err(|e| format!("CoCreateInstance(ApplicationActivationManager) 失败: {e}"))?;
        let arguments = format!("--parent {}", std::process::id());
        let pid = mgr
            .ActivateApplication(
                &HSTRING::from(HELPER_AUMID),
                &HSTRING::from(arguments),
                ACTIVATEOPTIONS(0),
            )
            .map_err(|e| format!("ActivateApplication(助手) 失败: {e}"))?;
        Ok(pid)
    }
}

/// 证书不受信任根导致的 HRESULT(CERT_E_UNTRUSTEDROOT)。
/// 整族 0x800Bxxxx 都是 Crypto32 的证书链信任问题,加进
/// `CurrentUser\TrustedPeople` 即可解决,故一并重试。
const CERT_E_UNTRUSTEDROOT: u32 = 0x800B0109;

/// 确保稀疏包身份已注册。
///
/// - 已注册 → 直接 `Ok(())`(注册是幂等的,同名更高版本会被接受)
/// - 未注册 → 找到 msix,向系统注册(ExternalLocation = exe 所在目录)
/// - 若注册因证书不受信任根失败 → 用 `certutil` 把 .cer 加进
///   `CurrentUser\TrustedPeople`(每用户、无需管理员),再重试一次
/// - 任何失败 → `Err(原因)`,由调用方降级
pub fn ensure_identity_registered() -> Result<(), String> {
    // 已注册则跳过(也避免重复注册把时间花在已注册的路径上)
    if is_registered() {
        return Ok(());
    }

    let msix = find_msix().ok_or_else(|| {
        "找不到 clipboard-identity.msix:请把它放在 exe 同目录、或 src-tauri/identity/ 下(见 build-identity.ps1)".to_string()
    })?;

    let exe_dir = exe_dir().ok_or_else(|| "取不到当前 exe 所在目录".to_string())?;

    match register(&msix, &exe_dir) {
        Ok(()) => Ok(()),
        Err((code, msg)) => {
            // 证书不被系统信任:导入到受信任存储后重试一次。
            // 重试不做时间门控——首次注册可能因系统做证书链校验而耗时数分钟,
            // 任何死线都会把「导入证书(真正的修复动作)后的关键重试」拦掉;
            // 重试本身至多发生一次,不会无限阻塞
            if is_untrusted_cert_error(code, &msg) {
                let cer = msix.with_extension("cer");
                if let Err(import_err) = import_cert_trusted_people(&cer) {
                    return Err(format!(
                        "注册因证书不受信任({:#010X})失败,且导入证书也失败: {import_err}; 原始错误: {msg}",
                        code
                    ));
                }
                return register(&msix, &exe_dir).map_err(|(_, m)| {
                    format!(
                        "导入证书到受信任存储后注册仍失败: {m}。\
                         提示:签名信任校验由 AppX 部署服务在系统上下文执行,\
                         CurrentUser 导入通常不够;需要管理员执行 \
                         `certutil -addstore TrustedPeople <cer>`(LocalMachine)后重试"
                    )
                });
            }
            Err(msg)
        }
    }
}

/// 同名包是否已注册(简化判断:FindPackagesByNamePublisher 有结果即视为已注册;
/// AddPackageByUriAsync 对同名更高版本会自动更新,旧版本同名注册也会被接受)
fn is_registered() -> bool {
    let Ok(pm) = PackageManager::new() else {
        return false;
    };
    let Ok(packages) = pm.FindPackagesByNamePublisher(
        &HSTRING::from(PACKAGE_NAME),
        &HSTRING::from(PACKAGE_PUBLISHER),
    ) else {
        return false;
    };
    // IIterable<Package> 实现了 IntoIterator;有任何一项即已注册
    packages.into_iter().next().is_some()
}

/// 已注册稀疏包的版本,与 AppxManifest.xml 中的 Version 保持一致
/// (改清单版本时必须同步改这里,否则会被误判为「过期」而反复重装)。
/// 四元组含 Revision:仅比 (Major,Minor,Build) 无法区分内容过期但版本
/// 三元组相同的注册(实测踩过:0.2.1.2 与 0.2.1.3 前三位相同)。
const EXPECTED_IDENTITY_VERSION: (u64, u64, u64, u64) = (0, 2, 1, 5);

/// 已注册的稀疏包是否「可以直接用」:版本与 EXPECTED_IDENTITY_VERSION 完全一致,
/// **且**包的实际根目录(ExternalLocation)就是当前 exe 所在目录。
///
/// 只比版本是不够的:调试构建(tauri dev)会把同名同版本的包注册到
/// `src-tauri\target\debug`,安装版再启动时版本一致 → 误判「已注册」,
/// 之后激活 sms-helper 实际指向 debug 目录;该目录被清理后,登录时就弹
/// 「Windows 找不到文件 ...\target\debug\sms-helper.exe」。
/// 版本不一致(含「清单元数据里 Executable 指向旧 exe」)同样返回 false,
/// 调用方应强制重注册 —— 否则旧注册会让新进程拿不到身份,触发
/// 「无身份 → 重启」死循环。
fn registered_version_matches() -> bool {
    let Ok(pm) = PackageManager::new() else {
        return false;
    };
    let Ok(packages) = pm.FindPackagesByNamePublisher(
        &HSTRING::from(PACKAGE_NAME),
        &HSTRING::from(PACKAGE_PUBLISHER),
    ) else {
        return false;
    };
    let Some(pkg) = packages.into_iter().next() else {
        return false;
    };
    let Ok(ver) = pkg.Id().and_then(|id| id.Version()) else {
        return false;
    };
    let version_ok = (
        ver.Major as u64,
        ver.Minor as u64,
        ver.Build as u64,
        ver.Revision as u64,
    ) == EXPECTED_IDENTITY_VERSION;
    if !version_ok {
        return false;
    }

    // 版本对上了,再核对包根目录。读不到(注册表被保护/结构变化)就退回
    // 旧行为,避免每次启动都做一次昂贵的重注册
    let Ok(full_name) = pkg.Id().and_then(|id| id.FullName()) else {
        return true;
    };
    let Some(root) = registered_root_folder(&full_name.to_string_lossy()) else {
        return true;
    };
    // 普通(非稀疏)安装的根目录在 WindowsApps 下,不是外部内容目录,不适用本判断
    if is_windowsapps_path(&root) {
        return true;
    }
    let Some(exe_dir) = exe_dir() else {
        return true;
    };
    same_dir(&root, &exe_dir)
}

/// 读已注册包的 `PackageRootFolder`(稀疏包的真实外部内容目录)。
/// 稀疏包的 `Package.InstalledLocation` 只给 WindowsApps 下的存根,
/// WinRT 拿不到外部位置,只能读包的注册表项。
fn registered_root_folder(package_full_name: &str) -> Option<PathBuf> {
    use winreg::enums::HKEY_CURRENT_USER;
    let subkey = format!(
        "Software\\Classes\\Local Settings\\Software\\Microsoft\\Windows\\CurrentVersion\\AppModel\\Repository\\Packages\\{}",
        package_full_name
    );
    winreg::RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(subkey)
        .ok()?
        .get_value::<String, _>("PackageRootFolder")
        .ok()
        .map(PathBuf::from)
}

/// 目录判等用的归一化:统一分隔符、转小写、去 `\\?\` 前缀、去尾部分隔符
fn normalize_dir(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
    s.trim_end_matches('\\').to_string()
}

fn same_dir(a: &Path, b: &Path) -> bool {
    normalize_dir(a) == normalize_dir(b)
}

fn is_windowsapps_path(p: &Path) -> bool {
    normalize_dir(p).starts_with(r"c:\program files\windowsapps")
}

#[cfg(test)]
mod tests {
    use super::{normalize_dir, same_dir};
    use std::path::Path;

    #[test]
    fn same_dir_ignores_case_slashes_and_trailing_separator() {
        assert!(same_dir(
            Path::new(r"D:\clipboard"),
            Path::new(r"d:/Clipboard/")
        ));
        assert!(same_dir(
            Path::new(r"\\?\D:\clipboard"),
            Path::new(r"D:\clipboard")
        ));
    }

    #[test]
    fn same_dir_rejects_debug_build_dir() {
        // 事故现场:包被注册到 debug 目录,安装目录在 D:\clipboard
        assert!(!same_dir(
            Path::new(r"D:\copy\clipboard-manager-tauri\src-tauri\target\debug"),
            Path::new(r"D:\clipboard")
        ));
    }

    #[test]
    fn normalize_dir_trims_trailing_separators() {
        assert_eq!(normalize_dir(Path::new(r"D:\clipboard\\")), "d:\\clipboard");
    }
}

/// 强制重注册:移除同名包后按当前 msix 重新注册。
/// 用于「已注册但内容过期」的场景(例如测试期改过清单 Executable)。
fn force_reregister(msix: &std::path::Path, external_loc: &std::path::Path) -> Result<(), String> {
    let pm = PackageManager::new().map_err(|e| format!("PackageManager::new 失败: {e}"))?;
    let Ok(packages) = pm.FindPackagesByNamePublisher(
        &HSTRING::from(PACKAGE_NAME),
        &HSTRING::from(PACKAGE_PUBLISHER),
    ) else {
        return Err("FindPackagesByNamePublisher 失败".to_string());
    };
    for pkg in packages {
        if let Ok(full_name) = pkg.Id().and_then(|id| id.FullName()) {
            // 必须等卸载真正完成再注册:RemovePackageAsync 是异步操作,
            // 丢弃返回值就立刻 AddPackageByUriAsync 会因同名包仍在而冲突失败
            if let Ok(op) = pm.RemovePackageAsync(&full_name) {
                let _ = op.get();
            }
        }
    }
    register(msix, external_loc)
        .map(|_| ())
        .map_err(|(code, msg)| format!("重注册失败 {:#010X}: {}", code, msg))
}

/// 订阅重试仍失败时的兜底:确认「已注册的稀疏包版本与当前清单一致」,
/// 不一致(过期)就强制重注册,然后调用方再尝试以 AUMID 重启应用。
pub fn reregister_if_stale() -> Result<(), String> {
    let msix = find_msix().ok_or_else(|| "找不到 clipboard-identity.msix".to_string())?;
    let exe_dir = exe_dir().ok_or_else(|| "取不到当前 exe 所在目录".to_string())?;
    if registered_version_matches() {
        return Ok(());
    }
    force_reregister(&msix, &exe_dir)
}

// ---------------------------------------------------------------------------
// 重启冷却:防止「无身份 → 自动重启」无限循环(环境原因导致身份始终拿不到时,
// 例如签名证书被清理、清单与安装目录错位)。60 秒内最多自动重启一次;
// 事件订阅成功后清除标记,后续启动恢复立即可重启。
// ---------------------------------------------------------------------------

const RELAUNCH_MARKER: &str = "clipboard-identity-relaunch.marker";
const RELAUNCH_COOLDOWN: Duration = Duration::from_secs(60);

fn marker_path() -> PathBuf {
    std::env::temp_dir().join(RELAUNCH_MARKER)
}

/// 距上次自动重启是否已过冷却期
#[allow(dead_code)]
pub fn relaunch_allowed() -> bool {
    match std::fs::metadata(marker_path()) {
        Ok(m) => {
            let Ok(modified) = m.modified() else {
                return true;
            };
            modified
                .elapsed()
                .map(|e| e >= RELAUNCH_COOLDOWN)
                .unwrap_or(true)
        }
        Err(_) => true, // 标记不存在:从未重启过或已被清除
    }
}

/// 记录一次自动重启(冷却期起点)
#[allow(dead_code)]
pub fn mark_relaunch() {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let _ = std::fs::write(marker_path(), ts.to_string());
}

/// 事件订阅成功后清除标记(身份确认可用,下次启动允许自动重启)
pub fn clear_relaunch_marker() {
    let _ = std::fs::remove_file(marker_path());
}

/// 注册稀疏包。成功返回 `Ok(())`;失败返回 `(HRESULT 低 32 位, 原因)`(code 为 0
/// 表示拿不到具体错误码)。
fn register(msix: &std::path::Path, external_loc: &std::path::Path) -> Result<(), (u32, String)> {
    let external_uri = match file_uri(external_loc) {
        Ok(u) => u,
        Err(e) => return Err((0, e)),
    };
    let package_uri = match file_uri(msix) {
        Ok(u) => u,
        Err(e) => return Err((0, e)),
    };

    let pm = match PackageManager::new() {
        Ok(pm) => pm,
        Err(e) => return Err((0, format!("PackageManager::new 失败: {e}"))),
    };
    let options = match AddPackageOptions::new() {
        Ok(o) => o,
        Err(e) => return Err((0, format!("AddPackageOptions::new 失败: {e}"))),
    };
    if let Err(e) = options.SetExternalLocationUri(&external_uri) {
        return Err((0, format!("SetExternalLocationUri 失败: {e}")));
    }

    let op = match pm.AddPackageByUriAsync(&package_uri, &options) {
        Ok(op) => op,
        Err(e) => return Err((e.code().0 as u32, format!("AddPackageByUriAsync 失败: {e}"))),
    };

    match op.get() {
        Ok(result) => {
            // 注册可能「完成」但返回非零 ExtendedErrorCode(例如 capability 清单校验错误)
            match result.ExtendedErrorCode() {
                Ok(hr) if hr.0 == 0 => Ok(()),
                Ok(hr) => {
                    let text = result
                        .ErrorText()
                        .ok()
                        .map(|t| t.to_string_lossy().to_string())
                        .unwrap_or_default();
                    Err((
                        hr.0 as u32,
                        format!(
                            "注册返回非零 ExtendedErrorCode {:#010X}: {}",
                            hr.0 as u32,
                            text
                        ),
                    ))
                }
                Err(e) => Err((0, format!("读取 DeploymentResult.ExtendedErrorCode 失败: {e}"))),
            }
        }
        Err(e) => {
            let code = e.code().0 as u32;
            Err((code, format!("AddPackageByUriAsync 完成但报错 {:#010X}: {e}", code)))
        }
    }
}

/// 把本地路径构造成 `file:///` URI(Uri::CreateUri 要求绝对 URI)
fn file_uri(path: &std::path::Path) -> Result<Uri, String> {
    let s = path.to_string_lossy().replace('\\', "/");
    let uri_str = if s.starts_with("file://") {
        s.to_string()
    } else {
        format!("file:///{s}")
    };
    Uri::CreateUri(&HSTRING::from(uri_str.clone()))
        .map_err(|e| format!("构造 file URI 失败({uri_str}): {e}"))
}

/// 证书不受信任根错误。可能包括:
/// - `0x800B0109`(CERT_E_UNTRUSTEDROOT)及其整族 `0x800Bxxxx`(Crypto32 证书链信任问题)
/// - APPX 部署错误 `0x87E80034`(APPX_E_CERT_NOT_TRUSTED)及其 `0x87E8xxxx` 族:
///   注册稀疏包时,签名证书不在受信任存储会报这个码,内层仍是 0x800B0109
/// - 兜底:错误信息里直接出现不受信任根证书的具体码或关键字
///
/// 这些都能通过把 .cer 加进 `CurrentUser\TrustedPeople`(及 `Root`)解决。
fn is_untrusted_cert_error(code: u32, msg: &str) -> bool {
    if code == CERT_E_UNTRUSTEDROOT || (code & 0xFFFF_0000) == 0x800B_0000 {
        return true;
    }
    if code == 0x87E8_0034 || (code & 0xFFFF_0000) == 0x87E8_0000 {
        return true;
    }
    let m = msg.to_ascii_lowercase();
    m.contains("0x800b0109") || m.contains("untrusted") || m.contains("not trusted") || m.contains("受信任")
}

/// 用 `certutil` 把 .cer 加入 `CurrentUser` 下的受信任存储。
///
/// 导入证书到受信任存储(带 LocalMachine 回退 CurrentUser):
/// - 本应用以 requireAdministrator 运行,**LocalMachine\TrustedPeople 必然成功**
///   —— 这是实测唯一有效的位置:AppX 部署服务(AppXSvc,LocalSystem)在系统
///   上下文做签名链校验,只看 LocalMachine 存储(2026-09-05 实测:只导入
///   CurrentUser\Root + TrustedPeople 仍报 0x800B0109);
/// - `-user`(CurrentUser)作为回退保留:万一运行在非提权环境(dev 探针等),
///   导入 CurrentUser 虽然通常不够,但记录错误比静默跳过好。
fn import_cert_trusted_people(cer: &std::path::Path) -> Result<(), String> {
    if !cer.is_file() {
        return Err(format!("找不到证书文件 {}", cer.display()));
    }
    let cer_str = cer.to_string_lossy().to_string();
    let mut last_err = String::new();

    // 1) LocalMachine(管理员必成功);含 Root + TrustedPeople 双保险
    let mut machine_ok = false;
    for store in ["Root", "TrustedPeople"] {
        let status = std::process::Command::new("certutil")
            .args(["-f", "-addstore", store, &cer_str])
            .status()
            .map_err(|e| format!("启动 certutil 失败: {e}"));
        match status {
            Ok(s) if s.success() => machine_ok = true,
            Ok(s) => last_err = format!("certutil -addstore {store} 退出码 {}", s.code().unwrap_or(-1)),
            Err(e) => last_err = e,
        }
    }
    if machine_ok {
        return Ok(());
    }

    // 2) 回退:CurrentUser(非提权环境;通常不足以通过 AppX 校验,仅记录)
    let mut user_ok = false;
    for store in ["Root", "TrustedPeople"] {
        let status = std::process::Command::new("certutil")
            .args(["-user", "-f", "-addstore", store, &cer_str])
            .status();
        match status {
            Ok(s) if s.success() => user_ok = true,
            Ok(s) => last_err = format!("certutil -user -addstore {store} 退出码 {}", s.code().unwrap_or(-1)),
            Err(e) => last_err = e.to_string(),
        }
    }
    if user_ok {
        Ok(())
    } else {
        Err(format!(
            "导入证书到受信任存储失败(LocalMachine 与 CurrentUser 均失败): {last_err}"
        ))
    }
}

/// 在候选位置里找 clipboard-identity.msix(优先级:exe 同目录 →
/// identity 子目录 → 仓库 src-tauri/identity,兼顾探针与正式运行两种布局)
fn find_msix() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let name = "clipboard-identity.msix";
    let candidates = [
        dir.join(name),
        dir.join("identity").join(name),
        dir.join("..").join("identity").join(name),
        dir.join("..")
            .join("src-tauri")
            .join("identity")
            .join(name),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

/// 当前 exe 所在目录(ExternalLocation 用,不含文件名)
fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
}
