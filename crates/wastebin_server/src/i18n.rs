use phf::phf_map;

use wastebin_core::expiration::{Expiration, Unit};

/// Languages the UI is translated into. English is the default and the
/// fallback for any key missing from a non-English table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Lang {
    #[default]
    En,
    De,
    Zh,
}

impl Lang {
    /// BCP-47 code suitable for the `lang` attribute on `<html>`.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::De => "de",
            Lang::Zh => "zh",
        }
    }

    /// Look up `key` in the current language, falling back to English and
    /// finally to the key itself if the key is unknown everywhere. Keys are
    /// expected to be string literals.
    pub(crate) fn t(self, key: &'static str) -> &'static str {
        let map: &phf::Map<&'static str, &'static str> = match self {
            Lang::En => &EN,
            Lang::De => &DE,
            Lang::Zh => &ZH,
        };

        map.get(key).or_else(|| EN.get(key)).copied().unwrap_or(key)
    }

    /// Look up `key` and substitute the `{0}` placeholder with `arg`'s
    /// `Display` representation.
    pub(crate) fn t_with(self, key: &'static str, arg: impl std::fmt::Display) -> String {
        self.t(key).replace("{0}", &arg.to_string())
    }

    /// Render an [`Expiration`]'s duration in the current language.
    pub(crate) fn duration(self, expiration: &Expiration) -> String {
        if expiration.duration.is_zero() {
            return self.t("duration.never").to_string();
        }

        expiration
            .components()
            .into_iter()
            .map(|(value, unit)| {
                let (singular, plural) = match unit {
                    Unit::Year => ("duration.year", "duration.years"),
                    Unit::Month => ("duration.month", "duration.months"),
                    Unit::Week => ("duration.week", "duration.weeks"),
                    Unit::Day => ("duration.day", "duration.days"),
                    Unit::Hour => ("duration.hour", "duration.hours"),
                    Unit::Minute => ("duration.minute", "duration.minutes"),
                    Unit::Second => ("duration.second", "duration.seconds"),
                };

                let key = if value == 1 { singular } else { plural };
                self.t_with(key, value)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

static EN: phf::Map<&'static str, &'static str> = phf_map! {
    "nav.home" => "home",
    "nav.upload" => "upload",
    "nav.delete" => "delete paste",
    "nav.download" => "download file",
    "nav.raw" => "display raw file",
    "nav.copy" => "copy to clipboard",
    "nav.qr" => "qr code",
    "nav.rendered" => "rendered view",
    "nav.source" => "source view",
    "nav.new_pad" => "new pad",
    "nav.copy_markdown" => "copy as markdown",
    "nav.settings" => "settings",
    "nav.sidebar" => "sidebar",
    "index.label.theme" => "theme",

    "theme.dark" => "dark mode",
    "theme.light" => "light mode",
    "theme.auto" => "auto mode",

    "index.placeholder.paste" => "paste, type, or drop a file here …",
    "index.drop" => "drop to load file",
    "index.label.title" => "title",
    "index.placeholder.title" => "untitled",
    "index.label.language" => "language",
    "index.aria.language" => "language",
    "index.placeholder.filter" => "filter …",
    "index.label.expires" => "expires",
    "index.label.options" => "options",
    "index.toggle.burn" => "burn after reading",
    "index.toggle.burn.hint" => "delete on first view",
    "index.toggle.encrypt" => "encrypt",
    "index.toggle.encrypt.hint" => "password-protect the paste",
    "index.placeholder.password" => "password",
    "index.stat.lines" => "lines",
    "index.stat.chars" => "chars",
    "index.stat.bytes" => "bytes",
    "index.button.paste" => "Paste",
    "index.button.paste.label" => "paste",
    "index.aria.editor" => "editor",
    "index.lang.auto" => "auto",
    "index.lang.document" => "document",
    "index.label.pads" => "pads",
    "index.pads.untitled" => "untitled",
    "index.pads.empty" => "no pads yet",
    "index.pad.delete" => "delete pad",
    "index.pad.confirm_delete" => "Delete this pad for everyone?",
    "index.confirm.drop_assets" => "Switching to a code language removes the images and PDFs from this pad. Continue?",
    "index.asset.open" => "open",
    "index.asset.download" => "download",
    "index.asset.remove" => "remove",
    "index.status.saving" => "saving …",
    "index.status.saved" => "saved",
    "index.status.live" => "live",
    "index.status.offline" => "offline",
    "index.status.uploading" => "uploading …",
    "index.status.deleted" => "pad was deleted",
    "index.status.error" => "error",
    "index.confirm.discard" => "This pad has unsaved changes. Discard them?",
    "index.status.too_large" => "too large",
    "index.status.conflict" => "conflict, reloaded",
    "index.toast.copied_markdown" => "Copied as Markdown",
    "index.toast.copy_failed" => "Could not copy",
    "index.toast.upload_failed" => "Upload failed",

    "paste.expires_in" => "expires in",
    "paste.toast.copied_content" => "Copied content",
    "paste.toast.copied_url" => "Copied URL",
    "paste.toast.burned" => "Content is burned and cannot be looked up again!",
    "paste.help.go_home" => "Go home",
    "paste.help.go_here" => "Go here",
    "paste.help.copy_url" => "Copy URL",
    "paste.help.copy_content" => "Copy content",
    "paste.help.download" => "Download",
    "paste.help.show_qr" => "Show QR code",
    "paste.help.toggle_wrap" => "Toggle line wrapping",
    "paste.help.toggle_rendered" => "Toggle rendered view",
    "paste.help.toggle_help" => "Toggle help",

    "password.show" => "show password",
    "password.hide" => "hide password",

    "stats.unit.kb" => "kb",
    "stats.unit.mb" => "mb",
    "stats.label.limit" => "limit",

    "duration.never" => "never",
    "duration.year" => "{0} year",
    "duration.years" => "{0} years",
    "duration.month" => "{0} month",
    "duration.months" => "{0} months",
    "duration.week" => "{0} week",
    "duration.weeks" => "{0} weeks",
    "duration.day" => "{0} day",
    "duration.days" => "{0} days",
    "duration.hour" => "{0} hour",
    "duration.hours" => "{0} hours",
    "duration.minute" => "{0} minute",
    "duration.minutes" => "{0} mins",
    "duration.second" => "{0} second",
    "duration.seconds" => "{0} secs",

    "burn.title" => "Burn after reading",
    "burn.body.before" => "Copy and send ",
    "burn.body.link" => "this link",
    "burn.body.after" => ". The recipient will be shown a confirmation prompt. The paste is deleted the moment they confirm.",

    "burn_confirm.body" => "This paste will be <strong>permanently deleted</strong> the moment it is revealed. You will not be able to view it again.",
    "burn_confirm.cancel" => "cancel",
    "burn_confirm.reveal" => "reveal",

    "encrypted.title" => "Encrypted paste",
    "encrypted.placeholder" => "password …",
    "encrypted.cancel" => "cancel",
    "encrypted.decrypt" => "decrypt",

    "error.title" => "Error 😢",
    "error.back" => "go back",

    "qr.label" => "qr code",
};

static DE: phf::Map<&'static str, &'static str> = phf_map! {
    "nav.home" => "Start",
    "nav.upload" => "Hochladen",
    "nav.delete" => "Paste löschen",
    "nav.download" => "Datei herunterladen",
    "nav.raw" => "Rohansicht",
    "nav.copy" => "In Zwischenablage kopieren",
    "nav.qr" => "QR-Code",
    "nav.rendered" => "Gerenderte Ansicht",
    "nav.source" => "Quelltext-Ansicht",
    "nav.new_pad" => "Neues Pad",
    "nav.copy_markdown" => "Als Markdown kopieren",
    "nav.settings" => "Einstellungen",
    "nav.sidebar" => "Seitenleiste",
    "index.label.theme" => "Design",

    "theme.dark" => "Dunkler Modus",
    "theme.light" => "Heller Modus",
    "theme.auto" => "Automatisch",

    "index.placeholder.paste" => "Text einfügen, tippen oder Datei hierher ziehen …",
    "index.drop" => "Datei hier ablegen",
    "index.label.title" => "Titel",
    "index.placeholder.title" => "ohne Titel",
    "index.label.language" => "Sprache",
    "index.aria.language" => "Sprache",
    "index.placeholder.filter" => "filtern …",
    "index.label.expires" => "Läuft ab",
    "index.label.options" => "Optionen",
    "index.toggle.burn" => "Nach Lesen vernichten",
    "index.toggle.burn.hint" => "Nach erstem Aufruf löschen",
    "index.toggle.encrypt" => "Verschlüsseln",
    "index.toggle.encrypt.hint" => "Paste mit Passwort schützen",
    "index.placeholder.password" => "Passwort",
    "index.stat.lines" => "Zeilen",
    "index.stat.chars" => "Zeichen",
    "index.stat.bytes" => "Bytes",
    "index.button.paste" => "Einfügen",
    "index.button.paste.label" => "einfügen",
    "index.aria.editor" => "Editor",
    "index.lang.auto" => "automatisch",
    "index.lang.document" => "Dokument",
    "index.label.pads" => "Pads",
    "index.pads.untitled" => "ohne Titel",
    "index.pads.empty" => "noch keine Pads",
    "index.pad.delete" => "Pad löschen",
    "index.pad.confirm_delete" => "Dieses Pad für alle löschen?",
    "index.confirm.drop_assets" => "Beim Wechsel zu einer Programmiersprache werden die Bilder und PDFs aus diesem Pad entfernt. Fortfahren?",
    "index.asset.open" => "öffnen",
    "index.asset.download" => "herunterladen",
    "index.asset.remove" => "entfernen",
    "index.status.saving" => "speichert …",
    "index.status.saved" => "gespeichert",
    "index.status.live" => "live",
    "index.status.offline" => "offline",
    "index.status.uploading" => "lädt hoch …",
    "index.status.deleted" => "Pad wurde gelöscht",
    "index.status.error" => "Fehler",
    "index.confirm.discard" => "Dieses Pad hat ungespeicherte Änderungen. Verwerfen?",
    "index.status.too_large" => "zu groß",
    "index.status.conflict" => "Konflikt, neu geladen",
    "index.toast.copied_markdown" => "Als Markdown kopiert",
    "index.toast.copy_failed" => "Kopieren fehlgeschlagen",
    "index.toast.upload_failed" => "Hochladen fehlgeschlagen",

    "paste.expires_in" => "läuft ab in",
    "paste.toast.copied_content" => "Inhalt kopiert",
    "paste.toast.copied_url" => "URL kopiert",
    "paste.toast.burned" => "Inhalt ist vernichtet und kann nicht mehr abgerufen werden!",
    "paste.help.go_home" => "Zur Startseite",
    "paste.help.go_here" => "Zu diesem Paste",
    "paste.help.copy_url" => "URL kopieren",
    "paste.help.copy_content" => "Inhalt kopieren",
    "paste.help.download" => "Herunterladen",
    "paste.help.show_qr" => "QR-Code anzeigen",
    "paste.help.toggle_wrap" => "Zeilenumbruch umschalten",
    "paste.help.toggle_rendered" => "Markdown Ansicht umschalten",
    "paste.help.toggle_help" => "Hilfe umschalten",

    "password.show" => "Passwort anzeigen",
    "password.hide" => "Passwort verbergen",

    "stats.unit.kb" => "kB",
    "stats.unit.mb" => "MB",
    "stats.label.limit" => "Limit",

    "duration.never" => "nie",
    "duration.year" => "{0} Jahr",
    "duration.years" => "{0} Jahre",
    "duration.month" => "{0} Monat",
    "duration.months" => "{0} Monate",
    "duration.week" => "{0} Woche",
    "duration.weeks" => "{0} Wochen",
    "duration.day" => "{0} Tag",
    "duration.days" => "{0} Tage",
    "duration.hour" => "{0} Stunde",
    "duration.hours" => "{0} Stunden",
    "duration.minute" => "{0} Minute",
    "duration.minutes" => "{0} Minuten",
    "duration.second" => "{0} Sekunde",
    "duration.seconds" => "{0} Sekunden",

    "burn.title" => "Nach Lesen vernichten",
    "burn.body.before" => "Kopiere und schicke ",
    "burn.body.link" => "diesen Link",
    "burn.body.after" => ". Dem Empfänger wird eine Bestätigungsaufforderung angezeigt und der Paste nach Bestätigung gelöscht.",

    "burn_confirm.body" => "Dieser Paste wird <strong>unwiderruflich gelöscht</strong>, sobald er angezeigt wird und kann danach nicht mehr eingesehen werden.",
    "burn_confirm.cancel" => "Abbrechen",
    "burn_confirm.reveal" => "Anzeigen",

    "encrypted.title" => "Verschlüsselter Paste",
    "encrypted.placeholder" => "Passwort …",
    "encrypted.cancel" => "Abbrechen",
    "encrypted.decrypt" => "Entschlüsseln",

    "error.title" => "Fehler 😢",
    "error.back" => "Zurück",

    "qr.label" => "QR-Code",
};

static ZH: phf::Map<&'static str, &'static str> = phf_map! {
    "nav.home" => "主页",
    "nav.upload" => "上传",
    "nav.delete" => "删除剪贴",
    "nav.download" => "下载文件",
    "nav.raw" => "显示原始内容",
    "nav.copy" => "复制到剪贴板",
    "nav.qr" => "二维码",
    "nav.rendered" => "渲染视图",
    "nav.source" => "源码视图",
    "nav.new_pad" => "新建笔记",
    "nav.copy_markdown" => "复制为 Markdown",
    "nav.settings" => "设置",
    "nav.sidebar" => "侧边栏",
    "index.label.theme" => "主题",

    "theme.dark" => "深色模式",
    "theme.light" => "浅色模式",
    "theme.auto" => "自动模式",

    "index.placeholder.paste" => "在此处粘贴、输入或拖放文件…",
    "index.drop" => "拖放以加载文件",
    "index.label.title" => "标题",
    "index.placeholder.title" => "无标题",
    "index.label.language" => "语言",
    "index.aria.language" => "语言",
    "index.placeholder.filter" => "过滤…",
    "index.label.expires" => "过期时间",
    "index.label.options" => "选项",
    "index.toggle.burn" => "阅后即焚",
    "index.toggle.burn.hint" => "首次查看后删除",
    "index.toggle.encrypt" => "加密",
    "index.toggle.encrypt.hint" => "使用密码保护剪贴",
    "index.placeholder.password" => "密码",
    "index.stat.lines" => "行",
    "index.stat.chars" => "字符",
    "index.stat.bytes" => "字节",
    "index.button.paste" => "粘贴",
    "index.button.paste.label" => "粘贴",
    "index.aria.editor" => "编辑器",
    "index.lang.auto" => "自动",
    "index.lang.document" => "文档",
    "index.label.pads" => "笔记",
    "index.pads.untitled" => "无标题",
    "index.pads.empty" => "暂无笔记",
    "index.pad.delete" => "删除笔记",
    "index.pad.confirm_delete" => "为所有人删除此笔记？",
    "index.confirm.drop_assets" => "切换到代码语言会移除此笔记中的图片和 PDF。是否继续？",
    "index.asset.open" => "打开",
    "index.asset.download" => "下载",
    "index.asset.remove" => "移除",
    "index.status.saving" => "保存中 …",
    "index.status.saved" => "已保存",
    "index.status.live" => "实时",
    "index.status.offline" => "离线",
    "index.status.uploading" => "上传中 …",
    "index.status.deleted" => "笔记已被删除",
    "index.status.error" => "错误",
    "index.confirm.discard" => "此笔记有未保存的更改。要放弃吗？",
    "index.status.too_large" => "内容过大",
    "index.status.conflict" => "冲突，已重新加载",
    "index.toast.copied_markdown" => "已复制为 Markdown",
    "index.toast.copy_failed" => "复制失败",
    "index.toast.upload_failed" => "上传失败",

    "paste.expires_in" => "过期于",
    "paste.toast.copied_content" => "已复制内容",
    "paste.toast.copied_url" => "已复制链接",
    "paste.toast.burned" => "内容已销毁，无法再次查看！",
    "paste.help.go_home" => "返回主页",
    "paste.help.go_here" => "返回此处",
    "paste.help.copy_url" => "复制链接",
    "paste.help.copy_content" => "复制内容",
    "paste.help.download" => "下载",
    "paste.help.show_qr" => "显示二维码",
    "paste.help.toggle_wrap" => "切换自动换行",
    "paste.help.toggle_rendered" => "切换渲染视图",
    "paste.help.toggle_help" => "切换帮助",

    "password.show" => "显示密码",
    "password.hide" => "隐藏密码",

    "stats.unit.kb" => "kb",
    "stats.unit.mb" => "mb",
    "stats.label.limit" => "限制",

    "duration.never" => "永不",
    "duration.year" => "{0} 年",
    "duration.years" => "{0} 年",
    "duration.month" => "{0} 个月",
    "duration.months" => "{0} 个月",
    "duration.week" => "{0} 周",
    "duration.weeks" => "{0} 周",
    "duration.day" => "{0} 天",
    "duration.days" => "{0} 天",
    "duration.hour" => "{0} 小时",
    "duration.hours" => "{0} 小时",
    "duration.minute" => "{0} 分钟",
    "duration.minutes" => "{0} 分钟",
    "duration.second" => "{0} 秒",
    "duration.seconds" => "{0} 秒",

    "burn.title" => "阅后即焚",
    "burn.body.before" => "复制并发送 ",
    "burn.body.link" => "此链接",
    "burn.body.after" => "。收件人将看到确认提示。在他们确认的那一刻，剪贴将被删除。",

    "burn_confirm.body" => "此剪贴在显示的那一刻将被 <strong>永久删除</strong>。您将无法再次查看它。",
    "burn_confirm.cancel" => "取消",
    "burn_confirm.reveal" => "显示",

    "encrypted.title" => "加密的剪贴",
    "encrypted.placeholder" => "密码…",
    "encrypted.cancel" => "取消",
    "encrypted.decrypt" => "解密",

    "error.title" => "错误 😢",
    "error.back" => "返回",

    "qr.label" => "二维码",
};

#[cfg(test)]
mod tests {
    use super::*;
    use itertools::Itertools;

    #[test]
    fn falls_back_to_english_for_missing_keys() {
        assert_eq!(Lang::De.t("nav.home"), "Start");
        assert_eq!(Lang::En.t("nav.home"), "home");
        // Unknown key returns the key itself.
        assert_eq!(Lang::De.t("does.not.exist"), "does.not.exist");
    }

    #[test]
    fn t_with_substitutes_placeholder() {
        assert_eq!(Lang::En.t_with("duration.days", "3"), "3 days");
        assert_eq!(Lang::De.t_with("duration.days", "3"), "3 Tage");
    }

    #[test]
    fn duration_is_localized() {
        let expiration: Expiration = "9000".parse().unwrap();
        assert_eq!(Lang::En.duration(&expiration), "2 hours, 30 mins");
        assert_eq!(Lang::De.duration(&expiration), "2 Stunden, 30 Minuten");
        assert_eq!(Lang::Zh.duration(&expiration), "2 小时, 30 分钟");

        let singular: Expiration = "1d".parse().unwrap();
        assert_eq!(Lang::En.duration(&singular), "1 day");
        assert_eq!(Lang::De.duration(&singular), "1 Tag");
        assert_eq!(Lang::Zh.duration(&singular), "1 天");

        let never: Expiration = "0=d".parse().unwrap();
        assert_eq!(Lang::En.duration(&never), "never");
        assert_eq!(Lang::De.duration(&never), "nie");
        assert_eq!(Lang::Zh.duration(&never), "永不");
    }

    #[test]
    fn translations_intersect() {
        for perms in [&DE, &EN, &ZH].iter().permutations(2) {
            let a = perms[0];
            let b = perms[1];

            for key in a.keys() {
                assert!(b.contains_key(key));
            }

            for key in b.keys() {
                assert!(a.contains_key(key));
            }
        }
    }
}
