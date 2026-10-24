//! Справка (`?`) и окно настроек (`o`): содержимое и правила изменения.

use crate::config::Config;
use crate::i18n::{Lang, Text};
use crate::ui::theme::ThemeName;

/// Раздел справки: заголовок и пары «клавиши — что делают».
pub type Section = (&'static str, Vec<(&'static str, &'static str)>);

/// Полная справка по клавишам, сгруппированная по режимам.
pub fn sections(t: &'static Text) -> Vec<Section> {
    vec![
        (
            t.sec_panels,
            vec![
                ("↑↓ PgUp PgDn Home End", t.hint_move),
                ("→  Enter", t.help_enter),
                ("←", t.help_up),
                ("Tab", t.hint_panel),
                ("<  >", t.hint_width),
                ("P", t.hint_hide),
                ("0 1 2 3", t.hint_columns),
                ("Shift+←→", t.help_columns_move),
                (".", t.hint_hidden),
                ("g", t.help_drives),
            ],
        ),
        (
            t.sec_files,
            vec![
                ("Space", t.hint_mark),
                ("Enter", t.help_open),
                ("c", t.help_copy),
                ("v", t.help_move),
                ("d", t.help_trash),
                ("D", t.help_delete),
                ("a", t.help_pack),
                ("r  F2", t.help_rename),
                ("m", t.help_mkdir),
                ("n", t.help_mkfile),
            ],
        ),
        (
            t.sec_view,
            vec![
                ("p", t.help_preview),
                ("↑↓ PgUp PgDn", t.hint_scroll),
                ("←→", t.help_shift),
                ("w", t.hint_wrap),
                ("/", t.help_view_search),
                ("e", t.hint_edit),
                ("Esc", t.hint_close),
            ],
        ),
        (
            t.sec_edit,
            vec![
                ("Ctrl+S", t.hint_save),
                ("Esc", t.help_exit_editor),
                ("Tab  Shift+Tab", t.hint_indent),
                ("Home", t.help_home),
                ("Ctrl+Home  Ctrl+End", t.help_file_ends),
            ],
        ),
        (
            t.sec_search,
            vec![
                ("/", t.help_grep),
                ("f", t.help_find),
                ("↑↓", t.hint_results),
                ("Enter", t.hint_goto),
                ("Esc", t.hint_exit),
            ],
        ),
        (
            t.sec_large,
            vec![
                ("l", t.help_large),
                ("→←", t.hint_in_out),
                ("d", t.help_trash),
                ("D", t.help_delete),
                ("Esc", t.help_large_esc),
            ],
        ),
        (
            t.sec_general,
            vec![
                ("?", t.hint_help),
                ("o", t.hint_settings),
                ("q", t.help_q),
                ("Ctrl+C", t.help_ctrl_c),
            ],
        ),
    ]
}

/// Число строк справки при показе: заголовки, пункты и пустые строки между разделами.
pub fn line_count(t: &'static Text) -> usize {
    sections(t).iter().map(|(_, items)| items.len() + 2).sum()
}

/// Поле окна настроек.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Lang,
    Theme,
    Hidden,
    Wrap,
    Indent,
    Trash,
}

impl Field {
    pub const ALL: [Field; 6] = [
        Field::Lang,
        Field::Theme,
        Field::Hidden,
        Field::Wrap,
        Field::Indent,
        Field::Trash,
    ];

    pub fn label(self, t: &Text) -> &'static str {
        match self {
            Field::Lang => t.set_language,
            Field::Theme => t.set_theme,
            Field::Hidden => t.set_hidden,
            Field::Wrap => t.set_wrap,
            Field::Indent => t.set_indent,
            Field::Trash => t.set_trash,
        }
    }

    /// Текущее значение поля для показа.
    pub fn value(self, cfg: &Config, t: &Text) -> String {
        let flag = |on: bool| if on { t.on } else { t.off }.to_string();
        match self {
            Field::Lang => cfg.lang.name().to_string(),
            Field::Theme => cfg.theme.name().to_string(),
            Field::Hidden => flag(cfg.show_hidden),
            Field::Wrap => flag(cfg.wrap),
            Field::Indent => cfg.indent.to_string(),
            Field::Trash => flag(cfg.use_trash),
        }
    }

    /// Сдвигает значение на `step` (±1): списки идут по кругу, флаги
    /// переключаются, отступ меняется в пределах 1..=8.
    pub fn change(self, cfg: &mut Config, step: isize) {
        fn cycle<T: Copy + PartialEq>(all: &[T], current: T, step: isize) -> T {
            let i = all.iter().position(|&x| x == current).unwrap_or(0) as isize;
            all[(i + step).rem_euclid(all.len() as isize) as usize]
        }
        match self {
            Field::Lang => cfg.lang = cycle(&Lang::ALL, cfg.lang, step),
            Field::Theme => cfg.theme = cycle(&ThemeName::ALL, cfg.theme, step),
            Field::Hidden => cfg.show_hidden = !cfg.show_hidden,
            Field::Wrap => cfg.wrap = !cfg.wrap,
            Field::Indent => cfg.indent = (cfg.indent as isize + step).clamp(1, 8) as u8,
            Field::Trash => cfg.use_trash = !cfg.use_trash,
        }
    }
}

/// Окно настроек: выбранное поле и значения на момент открытия — для отмены.
pub struct SettingsForm {
    pub field: usize,
    pub original: Config,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_covers_every_language() {
        for lang in Lang::ALL {
            let t = lang.text();
            let all = sections(t);
            assert_eq!(all.len(), 7);
            assert!(
                all.iter()
                    .all(|(title, items)| !title.is_empty() && !items.is_empty())
            );
            assert_eq!(
                line_count(t),
                all.iter().map(|(_, i)| i.len() + 2).sum::<usize>()
            );
        }
    }

    #[test]
    fn fields_change_values() {
        let mut cfg = Config::default();
        Field::Lang.change(&mut cfg, 1);
        assert_eq!(cfg.lang, Lang::Ru);
        Field::Lang.change(&mut cfg, 1);
        assert_eq!(cfg.lang, Lang::En);
        Field::Theme.change(&mut cfg, -1);
        assert_eq!(cfg.theme, ThemeName::Nord);
        Field::Hidden.change(&mut cfg, 1);
        assert!(!cfg.show_hidden);
        for _ in 0..20 {
            Field::Indent.change(&mut cfg, 1);
        }
        assert_eq!(cfg.indent, 8);
        for _ in 0..20 {
            Field::Indent.change(&mut cfg, -1);
        }
        assert_eq!(cfg.indent, 1);
        let t = Lang::Ru.text();
        assert_eq!(Field::Hidden.value(&cfg, t), "выкл");
        assert_eq!(Field::Lang.value(&cfg, t), "English");
    }
}
