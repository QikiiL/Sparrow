use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum CloseBehavior {
    Ask,
    Minimize,
    Close,
}

impl Default for CloseBehavior {
    fn default() -> Self {
        Self::Ask
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub start_with_windows: bool,
    pub retention_days: i32,
    pub max_item_count: i32,
    pub hotkey_modifier: String,
    pub hotkey_key: String,
    // 连续粘贴的步进热键:每按一次从 FIFO 队列头出队一条并粘贴。
    // 默认 Ctrl+V —— 热键仅在队列存续期间注册为全局热键,队列结束立即注销,
    // 日常粘贴不受影响;开始连贴后按 Ctrl+V 即"粘贴下一条",与手动粘贴同键自然衔接
    #[serde(default = "default_seq_paste_modifier")]
    pub seq_paste_modifier: String,
    #[serde(default = "default_seq_paste_key")]
    pub seq_paste_key: String,
    pub paused: bool,
    // 兼容旧版 settings.json:后加的字段缺省时不应导致整个结构体反序列化失败
    #[serde(default)]
    pub close_behavior: CloseBehavior,
    #[serde(default)]
    pub win_v_integration: bool,
    #[serde(default = "default_pinned")]
    pub pinned: bool,
    // 「粘贴后保持打开」:开启后单击条目不再销毁面板,只把前台焦点还给
    // 目标窗口。默认关闭(保持"隐藏即销毁"的低占用语义)
    #[serde(default)]
    pub keep_open_on_paste: bool,
    // 排除规则三件套,同样是为了兼容旧版 settings.json
    #[serde(default)]
    pub excluded_apps: Vec<String>,
    #[serde(default)]
    pub excluded_patterns: Vec<String>,
    #[serde(default = "default_true")]
    pub detect_sensitive: bool,
    // 误伤豁免:用户在「已排除」提示上点过「仍要记录」的内容 hash。
    // 没有它,被误判的内容永远进不了历史——再复制一次还是命中同一条规则
    #[serde(default)]
    pub excluded_allowlist: Vec<String>,
    // 短信验证码自动捕获。默认关闭:开启需要用户去系统设置里授予
    // 通知访问权限,由设置面板引导完成后再真正启用轮询
    #[serde(default)]
    pub sms_code_enabled: bool,
    // 云同步配置。整对象覆盖写(前端 save_settings 回传全量对象),
    // 缺省安全:旧版 settings.json 没有 sync 字段时回落 SyncSettings::default()
    #[serde(default)]
    pub sync: crate::models::sync::SyncSettings,
}

/// 窗口默认置顶;serde 的字段级 default 若不指定函数,bool 恒为 false,
/// 老配置文件缺 pinned 字段时会退回 false,故显式指定
fn default_pinned() -> bool {
    true
}

/// 敏感识别默认开启;同 default_pinned 的理由——不指定函数时 bool 恒为 false,
/// 等于给老用户静默关掉一个安全特性
fn default_true() -> bool {
    true
}

fn default_seq_paste_modifier() -> String {
    "Ctrl".to_string()
}

fn default_seq_paste_key() -> String {
    "V".to_string()
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            start_with_windows: false,
            retention_days: 30,
            max_item_count: 500,
            hotkey_modifier: "Ctrl+Shift".to_string(),
            hotkey_key: "V".to_string(),
            seq_paste_modifier: "Ctrl".to_string(),
            seq_paste_key: "V".to_string(),
            paused: false,
            close_behavior: CloseBehavior::default(),
            win_v_integration: false,
            pinned: true,
            keep_open_on_paste: false,
            // 预置常见密码管理器:从密码管理器复制出来的密码是明文泄漏的头号
            // 来源,默认拦掉比等用户自己发现设置项更稳妥
            excluded_apps: vec![
                "keepass.exe".to_string(),
                "keepassxc.exe".to_string(),
                "1password.exe".to_string(),
                "bitwarden.exe".to_string(),
            ],
            excluded_patterns: Vec::new(),
            detect_sensitive: true,
            excluded_allowlist: Vec::new(),
            sms_code_enabled: false,
            sync: crate::models::sync::SyncSettings::default(),
        }
    }
}

impl AppSettings {
    /// 连续粘贴步进热键的一次性迁移:首个版本默认 Ctrl+Alt+V,随后默认改为
    /// Ctrl+V(仅队列存续期间注册为全局热键,与手动粘贴同键更顺手)。
    /// 已保存配置里「仍是旧默认值」的直接跟随新默认;用户自定义过的其他
    /// 组合一律不动。由 settings_service::load_settings 在每次读取时调用。
    pub fn migrate_legacy_seq_hotkey(&mut self) {
        if self.seq_paste_modifier == "Ctrl+Alt" && self.seq_paste_key == "V" {
            self.seq_paste_modifier = "Ctrl".to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_settings() {
        let settings = AppSettings::default();
        assert_eq!(settings.retention_days, 30);
        assert_eq!(settings.max_item_count, 500);
        assert!(!settings.paused);
        assert_eq!(settings.close_behavior, CloseBehavior::Ask);
        assert!(!settings.win_v_integration);
        assert!(!settings.keep_open_on_paste);
        assert_eq!(settings.seq_paste_modifier, "Ctrl");
        assert_eq!(settings.seq_paste_key, "V");
    }

    #[test]
    fn legacy_seq_hotkey_migrates_to_ctrl_v() {
        // 仍是旧默认 Ctrl+Alt+V 的配置 → 跟随新默认 Ctrl+V
        let mut legacy = AppSettings::default();
        legacy.seq_paste_modifier = "Ctrl+Alt".to_string();
        legacy.migrate_legacy_seq_hotkey();
        assert_eq!(legacy.seq_paste_modifier, "Ctrl");
        assert_eq!(legacy.seq_paste_key, "V");

        // 用户自定义过的其他组合不受迁移影响
        let mut custom = AppSettings::default();
        custom.seq_paste_modifier = "Ctrl+Alt".to_string();
        custom.seq_paste_key = "P".to_string();
        custom.migrate_legacy_seq_hotkey();
        assert_eq!(custom.seq_paste_modifier, "Ctrl+Alt");
        assert_eq!(custom.seq_paste_key, "P");

        // 已经是新默认的配置原样保留
        let mut current = AppSettings::default();
        current.migrate_legacy_seq_hotkey();
        assert_eq!(current.seq_paste_modifier, "Ctrl");
    }

    #[test]
    fn test_settings_serialization() {
        let settings = AppSettings::default();
        let json = serde_json::to_string(&settings).unwrap();
        let deserialized: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.retention_days, settings.retention_days);
    }

    /// 旧版 settings.json 里没有 sync 字段(或前端回传的对象漏了这个字段):
    /// 反序列化必须回落默认值,不能让整份设置读取失败。
    #[test]
    fn missing_sync_field_uses_default() {
        let mut value = serde_json::to_value(AppSettings::default()).unwrap();
        value.as_object_mut().unwrap().remove("sync");
        let settings: AppSettings = serde_json::from_value(value).unwrap();
        assert!(!settings.sync.enabled);
        assert_eq!(settings.sync.root_prefix, "sparrow");
        assert_eq!(settings.sync.poll_interval_sec, 5);
    }

    #[test]
    fn test_close_behavior_serde() {
        assert_eq!(
            serde_json::to_string(&CloseBehavior::Ask).unwrap(),
            "\"ask\""
        );
        assert_eq!(
            serde_json::to_string(&CloseBehavior::Minimize).unwrap(),
            "\"minimize\""
        );
        assert_eq!(
            serde_json::to_string(&CloseBehavior::Close).unwrap(),
            "\"close\""
        );
        assert_eq!(
            serde_json::from_str::<CloseBehavior>("\"ask\"").unwrap(),
            CloseBehavior::Ask
        );
        assert_eq!(
            serde_json::from_str::<CloseBehavior>("\"minimize\"").unwrap(),
            CloseBehavior::Minimize
        );
        assert_eq!(
            serde_json::from_str::<CloseBehavior>("\"close\"").unwrap(),
            CloseBehavior::Close
        );
    }
}
