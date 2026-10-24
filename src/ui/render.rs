//! Отрисовка: две панели, строка подсказок и диалоги поверх них.

use chrono::{DateTime, Local};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::Side;
use crate::highlight::Piece;
use crate::i18n::Text;
use crate::listing::{Entry, Kind};
use crate::ops::Progress;
use crate::preview::{Body, DirInfo, Preview, TextBody};
use crate::search::Kind as SearchKind;
use crate::ui::finder::Finder;
use crate::ui::job::{Job, JobKind};
use crate::ui::theme::Theme;
use crate::ui::viewer::{Search, Viewer};
use crate::ui::{App, Mode};

/// Ширина колонки размера в однострочном режиме: `1023.9 K`.
const SIZE_WIDTH: usize = 8;
/// Ширина колонки даты: `2026-10-10 12:34`.
const DATE_WIDTH: usize = 16;
/// Меньше этого под имя не отдаём: без имени размер и дата бесполезны.
const NAME_MIN: usize = 12;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(app.theme.base()), area);

    // Подсказки занимают одну строку, а если не влезают — две.
    let hint_lines = hint_rows(app, (area.width as usize).saturating_sub(1));
    let height = hint_lines.len().max(1) as u16;
    let [main, hints] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(height)]).areas(area);

    match app.hidden {
        _ if app.large.is_some() => draw_large(frame, main, app),
        Some(hidden) => draw_side(frame, main, app, hidden.other()),
        None => {
            let [left, right] = Layout::horizontal([
                Constraint::Percentage(app.left_pct),
                Constraint::Percentage(100 - app.left_pct),
            ])
            .areas(main);
            draw_side(frame, left, app, Side::Left);
            draw_side(frame, right, app, Side::Right);
        }
    }

    draw_hints(frame, hints, app, hint_lines);

    match &app.mode {
        Mode::Normal => {}
        Mode::ConfirmDelete { permanent, targets } => {
            draw_confirm(frame, main, *permanent, targets, &app.theme, app.text());
        }
        Mode::Job(job) => draw_job(frame, main, job, &app.theme, app.text()),
        Mode::EditorSave | Mode::EditorExit => draw_save(frame, main, app),
        Mode::Pack(form) => draw_pack(frame, main, form, &app.theme, app.text()),
        Mode::Help { scroll } => draw_help(frame, area, *scroll, &app.theme, app.text()),
        Mode::Settings(form) => draw_settings(frame, main, form.field, app),
        Mode::Drives(form) => draw_drives(frame, main, form, &app.theme, app.text()),
        Mode::Rename(form) => draw_rename(frame, main, form, &app.theme, app.text()),
        Mode::Create(form) => draw_create(frame, main, form, &app.theme, app.text()),
    }
}

/// Сторона окна: файловая панель или просмотр на её месте.
fn draw_side(frame: &mut Frame, area: Rect, app: &mut App, side: Side) {
    if app.viewer.as_ref().is_some_and(|v| v.side == side) {
        if app.editor.is_some() {
            draw_editor(frame, area, app, side);
        } else {
            draw_viewer(frame, area, app, side);
        }
    } else if app.finder.as_ref().is_some_and(|f| f.side == side) {
        draw_finder(frame, area, app);
    } else {
        draw_panel(frame, area, app, side);
    }
}

fn draw_panel(frame: &mut Frame, area: Rect, app: &mut App, side: Side) {
    let active = app.active == side;
    let theme = app.theme;
    let text = app.text();

    let title_width = (area.width as usize).saturating_sub(4);
    let title = shorten_path(&display_path(&app.panel(side).display_dir()), title_width);
    let panel = app.panel(side);
    let counter = if panel.entries.is_empty() {
        String::new()
    } else {
        format!(" {}/{} ", panel.cursor + 1, panel.entries.len())
    };

    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border(active))
        .title(Line::styled(format!(" {title} "), theme.title(active)))
        .title_bottom(Line::styled(counter, theme.dim()).right_aligned());
    if !panel.marked.is_empty() {
        let marked = format!(
            " {}: {} · {} ",
            text.marked,
            panel.marked.len(),
            format_size(panel.marked_size())
        );
        block = block.title_bottom(Line::styled(marked, theme.marked()).left_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let panel = app.panel_mut(side);
    if panel.entries.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {}", text.empty), theme.dim())),
            inner,
        );
        return;
    }

    let rows = inner.height as usize;
    panel.fit(rows);
    let panel = app.panel(side);
    let columns = panel.layout_columns();

    // Колонки равной ширины, разделённые вертикальной чертой.
    let mut constraints = Vec::with_capacity(columns * 2);
    for i in 0..columns {
        if i > 0 {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Fill(1));
    }
    let cells = Layout::horizontal(constraints).split(inner);

    for col in 0..columns {
        let rect = cells[col * 2];
        if col > 0 {
            let sep = cells[col * 2 - 1];
            let lines = vec![Line::styled("│", theme.border(false)); sep.height as usize];
            frame.render_widget(Paragraph::new(lines), sep);
        }
        let start = panel.offset + col * rows;
        let lines: Vec<Line> = panel
            .entries
            .iter()
            .enumerate()
            .skip(start)
            .take(rows)
            .map(|(i, entry)| {
                let cursor = (i == panel.cursor).then_some(active);
                let look = Look {
                    details: panel.details(),
                    cursor,
                    marked: panel.is_marked(entry),
                    vcs_dirty: panel.is_vcs_dirty(entry),
                };
                entry_line(entry, rect.width as usize, look, &theme, text)
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), rect);
    }
}

/// Как показать элемент.
#[derive(Debug, Clone, Copy)]
struct Look {
    /// Режим одной колонки: справа размер и дата, если хватает места.
    details: bool,
    /// `Some(активна ли панель)`, если на элементе стоит курсор.
    cursor: Option<bool>,
    marked: bool,
    /// Есть незакоммиченные изменения в git.
    vcs_dirty: bool,
}

/// Строка одного элемента шириной ровно `width`.
fn entry_line<'a>(entry: &Entry, width: usize, look: Look, theme: &Theme, text: &Text) -> Line<'a> {
    let Look {
        details,
        cursor,
        marked,
        vcs_dirty,
    } = look;
    // Первая позиция — отметка выделения.
    let prefix = if marked { "*" } else { " " };
    let avail = width.saturating_sub(prefix.len());

    let mut right = String::new();
    if details {
        let size = size_label(entry, text);
        let date = entry.modified.map(format_time).unwrap_or_default();
        // Справа: пробел, размер, пробел, дата, пробел.
        let with_date = NAME_MIN + SIZE_WIDTH + DATE_WIDTH + 3;
        let with_size = NAME_MIN + SIZE_WIDTH + 2;
        if avail >= with_date {
            right = format!(" {size:>SIZE_WIDTH$} {date:<DATE_WIDTH$} ");
        } else if avail >= with_size {
            right = format!(" {size:>SIZE_WIDTH$} ");
        }
    }

    let name_width = avail.saturating_sub(right.width());
    let suffix = if entry.kind.is_dir() { "/" } else { "" };
    // В подробном режиме у ссылки видно, куда она ведёт; цель обрезается
    // первой, имя — только если не влезает даже оно само.
    let name = match (&entry.target, details && entry.kind.is_link()) {
        (Some(target), true) if entry.name.width() + suffix.width() < name_width => {
            let head = format!("{}{suffix}", entry.name);
            let tail = fit(
                &format!(" → {}", target.display()),
                "",
                name_width - head.width(),
            );
            format!("{head}{tail}")
        }
        _ => fit(&entry.name, suffix, name_width),
    };
    let pad = name_width.saturating_sub(name.width());

    // Выделение перекрашивает строку целиком: его должно быть видно сразу.
    let (name_style, right_style) = if marked {
        (theme.marked(), theme.marked())
    } else {
        let style = name_style(entry, theme);
        let style = if vcs_dirty {
            style.patch(theme.vcs_dirty())
        } else {
            style
        };
        (style, theme.dim())
    };
    let mut spans = vec![
        Span::styled(prefix, theme.marked()),
        Span::styled(name, name_style),
        Span::raw(" ".repeat(pad)),
    ];
    if !right.is_empty() {
        spans.push(Span::styled(right, right_style));
    }

    let line = Line::from(spans);
    match cursor {
        Some(active) => line.patch_style(theme.cursor(active)),
        None => line,
    }
}

/// Стиль имени по виду элемента.
fn name_style(entry: &Entry, theme: &Theme) -> Style {
    match entry.kind {
        Kind::Dir => theme.dir(),
        Kind::LinkDir => theme.link().patch(theme.dir()),
        Kind::LinkFile => theme.link(),
        Kind::LinkBroken => theme.broken(),
        Kind::File if entry.exec => theme.exec(),
        Kind::File | Kind::Special => Style::default(),
    }
}

/// Подпись в колонке размера.
fn size_label(entry: &Entry, text: &Text) -> String {
    match entry.kind {
        Kind::Dir => text.dir_label.to_string(),
        Kind::LinkDir => text.link_dir_label.to_string(),
        _ => format_size(entry.size),
    }
}

/// Компактный размер в двоичных единицах: `512 B`, `1.5 K`, `23.0 M`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

fn format_time(t: std::time::SystemTime) -> String {
    DateTime::<Local>::from(t)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Вписывает имя с суффиксом в `width` колонок терминала. Длинное имя
/// обрезается многоточием, суффикс (`/` директории) сохраняется всегда.
pub fn fit(name: &str, suffix: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if name.width() + suffix.width() <= width {
        return format!("{name}{suffix}");
    }
    let budget = width.saturating_sub(suffix.width() + 1);
    let mut out = String::new();
    let mut used = 0;
    for ch in name.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out.push_str(suffix);
    // Суффикс мог не уместиться даже сам по себе в совсем узкой колонке.
    truncate_width(&out, width)
}

/// Отрезает строку по ширине без многоточия.
fn truncate_width(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// Путь для заголовка: домашняя директория заменяется на `~`.
fn display_path(path: &std::path::Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

/// Укорачивает путь слева: конец пути важнее начала.
pub fn shorten_path(path: &str, width: usize) -> String {
    if path.width() <= width {
        return path.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut tail: Vec<char> = Vec::new();
    let mut used = 1; // многоточие
    for ch in path.chars().rev() {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        tail.push(ch);
        used += w;
    }
    let mut out = String::from("…");
    out.extend(tail.into_iter().rev());
    out
}

/// Строка подсказок. Сообщение о результате действия заменяет её первую строку.
fn draw_hints(frame: &mut Frame, area: Rect, app: &App, mut lines: Vec<Line<'static>>) {
    let theme = app.theme;
    let area = area.inner(Margin::new(1, 0));
    if let Some(status) = &app.status {
        let style = if status.error {
            theme.error()
        } else {
            Style::default()
        };
        let text = truncate_width(&status.text, area.width as usize);
        let line = Line::styled(text, style);
        if lines.is_empty() {
            lines.push(line);
        } else {
            lines[0] = line;
        }
    }
    frame.render_widget(Paragraph::new(lines), area);
}

/// Строки подсказок для текущего состояния: одна или две.
fn hint_rows(app: &App, width: usize) -> Vec<Line<'static>> {
    let theme = app.theme;
    let t = app.text();
    // Поиск показывает строку ввода — она всегда одна.
    if matches!(app.mode, Mode::Normal) && app.large.is_none() {
        if let Some(finder) = &app.finder {
            return vec![finder_line(finder, width, &theme, t)];
        }
        if app.viewer_focused()
            && let Some(search) = app.viewer.as_ref().and_then(|v| v.search.as_ref())
        {
            return vec![search_line(search, width, &theme, t)];
        }
    }
    wrap_hints(&mode_hints(app), width, &theme)
}

/// Раскладывает подсказки по строкам: что не влезло в первую, идёт во вторую;
/// не влезшее и во вторую заменяется многоточием — полный список в справке.
fn wrap_hints(
    hints: &[(&'static str, &'static str)],
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut used = 0;
    let mut split = hints.len();
    for (i, (key, desc)) in hints.iter().enumerate() {
        let item = key.width() + 1 + desc.width() + 2;
        if used + item > width {
            split = i;
            break;
        }
        used += item;
    }
    if split == hints.len() || split == 0 {
        return vec![hint_line(hints, width, theme)];
    }
    vec![
        hint_line(&hints[..split], width, theme),
        hint_line(&hints[split..], width, theme),
    ]
}

/// Клавиши текущего режима — только те, что сейчас что-то сделают.
fn mode_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let t = app.text();
    match &app.mode {
        Mode::Normal if app.large.is_some() => large_hints(app),
        Mode::Normal if app.editor.is_some() => vec![
            ("Ctrl+S", t.hint_save),
            ("Esc", t.hint_exit),
            ("Tab", t.hint_indent),
            ("Shift+Tab", t.hint_dedent),
        ],
        Mode::Normal if app.viewer_focused() => viewer_hints(app),
        Mode::Normal => files_hints(app),
        Mode::EditorSave => vec![("y", t.save), ("Esc q", t.cancel)],
        Mode::EditorExit => vec![("y", t.save), ("n", t.discard), ("Esc q", t.keep_editing)],
        Mode::Pack(_) => pack_hints(t),
        Mode::Help { .. } => vec![("↑↓", t.hint_scroll), ("Esc q", t.hint_close)],
        Mode::Settings(_) => settings_hints(t),
        Mode::Drives(_) => drives_hints(t),
        Mode::Rename(_) => rename_hints(t),
        Mode::Create(form) => create_hints(form.is_dir, t),
        Mode::ConfirmDelete { permanent, .. } => vec![
            (if *permanent { "D" } else { "d" }, t.confirm),
            ("Esc q", t.cancel),
        ],
        Mode::Job(job) if job.conflict.is_some() => conflict_hints(t).concat(),
        Mode::Job(_) => vec![("Esc q", t.cancel)],
    }
}

/// Подсказки файловой панели с учётом того, что сейчас возможно.
fn files_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let t = app.text();
    let panel = app.panel(app.active);
    let has = !panel.entries.is_empty();
    let two = app.hidden.is_none();
    let here_writable = !crate::panel::in_archive(&panel.dir);
    // Копировать и переносить есть куда: вторая панель видна, это не
    // просмотр, не архив и не та же директория.
    let other = app.panel(app.active.other());
    let can_transfer = has
        && two
        && app.viewer.is_none()
        && other.dir != panel.dir
        && !crate::panel::in_archive(&other.dir);

    let mut hints = vec![("?", t.hint_help)];
    if has {
        hints.push(("↑↓", t.hint_move));
    }
    hints.push(("→←", t.hint_in_out));
    if two {
        hints.push(("Tab", t.hint_panel));
    }
    if has {
        hints.push(("Space", t.hint_mark));
    }
    if can_transfer {
        hints.push(("c", t.hint_copy));
        hints.push(("v", t.hint_move_to));
    }
    if has {
        hints.push(("d", t.hint_delete));
    }
    hints.push(("p", t.hint_preview));
    hints.push(("/", t.hint_search));
    hints.push(("f", t.hint_find));
    if has && here_writable {
        hints.push(("r", t.hint_rename));
        hints.push(("a", t.hint_pack));
    }
    hints.push(("l", t.hint_large));
    if panel.columns > 1 && has {
        hints.push(("Shift+←→", t.hint_columns));
    }
    hints.push(("P", if two { t.hint_hide } else { t.hint_show }));
    if two {
        hints.push(("<>", t.hint_width));
    }
    hints.push(("0-3", t.hint_columns));
    hints.push(("o", t.hint_settings));
    hints.push(("q", t.hint_quit));
    // Последними: в узком окне уступают место выходу, полный список — в справке.
    if here_writable {
        hints.push(("m", t.hint_mkdir));
        hints.push(("n", t.hint_mkfile));
    }
    hints.push((".", t.hint_hidden));
    hints.push(("g", t.hint_drives));
    hints
}

/// Подсказки просмотра: зависят от того, что в нём показано.
fn viewer_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let t = app.text();
    let Some(viewer) = &app.viewer else {
        return Vec::new();
    };
    let text = matches!(
        viewer.content.as_ref().map(|c| &c.body),
        Some(Body::Text(_))
    );
    let mut hints = vec![("?", t.hint_help)];
    if viewer.content.is_some() {
        hints.push(("↑↓", t.hint_scroll));
    }
    if text && !viewer.wrap {
        hints.push(("←→", t.hint_shift));
    }
    if text
        && viewer
            .path
            .as_deref()
            .is_some_and(|p| !crate::panel::in_archive(p))
    {
        hints.push(("e", t.hint_edit));
    }
    if text {
        hints.push(("w", t.hint_wrap));
        hints.push(("/", t.hint_search));
    }
    hints.push(("Tab", t.hint_panel));
    hints.push(("Esc q", t.hint_close));
    hints
}

/// Подсказки режима «большие объекты».
fn large_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let t = app.text();
    let mut hints = vec![("?", t.hint_help)];
    if app.large.as_ref().is_some_and(|l| !l.order.is_empty()) {
        hints.push(("↑↓", t.hint_move));
        hints.push(("→←", t.hint_in_out));
        hints.push(("d", t.hint_delete));
    }
    hints.push(("Esc q", t.hint_exit));
    hints
}

/// Ответы на конфликт имён двумя строками: частые и «для всех».
fn conflict_hints(t: &'static Text) -> [Vec<(&'static str, &'static str)>; 2] {
    [
        vec![("r", t.replace), ("s", t.skip), ("Esc q", t.cancel)],
        vec![("R", t.replace_all), ("S", t.skip_all)],
    ]
}

/// Справка по всем клавишам поверх экрана, с прокруткой.
fn draw_help(frame: &mut Frame, area: Rect, scroll: usize, theme: &Theme, t: &'static Text) {
    let sections = crate::ui::help::sections(t);
    let key_width = sections
        .iter()
        .flat_map(|(_, items)| items.iter().map(|(k, _)| k.width()))
        .max()
        .unwrap_or(0)
        + 2;
    let mut lines = Vec::new();
    for (title, items) in &sections {
        lines.push(Line::styled(title.to_string(), theme.title(true)));
        for (key, desc) in items {
            lines.push(Line::from(vec![
                Span::styled(format!("  {key:<key_width$}"), theme.key()),
                Span::raw(desc.to_string()),
            ]));
        }
        lines.push(Line::raw(""));
    }
    // Оставляем по краям место, чтобы было видно, что это окно поверх панелей.
    let width = area.width.saturating_sub(4).min(90);
    let height = area.height.saturating_sub(2);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border(true))
        .title(Line::styled(
            format!(" {} ", t.help_title),
            theme.title(true),
        ))
        .title_bottom(
            Line::styled(
                format!(" {}/{} ", (scroll + 1).min(lines.len()), lines.len()),
                theme.dim(),
            )
            .right_aligned(),
        )
        .padding(Padding::horizontal(1))
        .style(theme.base());
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .scroll((scroll as u16, 0)),
        rect,
    );
}

/// Клавиши окна настроек.
fn settings_hints(t: &'static Text) -> Vec<(&'static str, &'static str)> {
    vec![
        ("↑↓", t.hint_move),
        ("←→", t.hint_change),
        ("Enter", t.save),
        ("Esc q", t.cancel),
    ]
}

/// Окно настроек: поле слева, значение справа, выбранное поле подсвечено.
fn draw_settings(frame: &mut Frame, area: Rect, selected: usize, app: &App) {
    let theme = app.theme;
    let t = app.text();
    let fields = crate::ui::help::Field::ALL;
    let label_width = fields.iter().map(|f| f.label(t).width()).max().unwrap_or(0) + 3;
    let value_width = 14;
    let body: Vec<Line> = fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let value = format!("‹ {} ›", field.value(&app.cfg, t));
            let line = Line::from(vec![
                Span::raw(format!("{:<label_width$}", field.label(t))),
                Span::styled(format!("{value:^value_width$}"), theme.key()),
            ]);
            if i == selected {
                line.patch_style(theme.cursor(true))
            } else {
                line
            }
        })
        .collect();
    // Клавиши окна — в строке подсказок внизу экрана.
    dialog(
        frame,
        area,
        t.settings_title,
        body,
        theme.border(true),
        &theme,
    );
}

/// Клавиши формы переименования.
fn rename_hints(t: &'static Text) -> Vec<(&'static str, &'static str)> {
    vec![("Enter", t.hint_rename), ("Esc", t.cancel)]
}

/// Форма переименования: прежнее имя и строка ввода с курсором. Длинное имя
/// прокручивается так, чтобы курсор оставался виден.
fn draw_rename(
    frame: &mut Frame,
    area: Rect,
    form: &crate::ui::RenameForm,
    theme: &Theme,
    t: &'static Text,
) {
    let old = form
        .source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let hints = rename_hints(t);
    draw_name_input(
        frame,
        area,
        t.rename_title,
        &old,
        &form.input,
        &hints,
        theme,
    );
}

/// Клавиши формы создания.
fn create_hints(is_dir: bool, t: &'static Text) -> Vec<(&'static str, &'static str)> {
    let what = if is_dir { t.hint_mkdir } else { t.hint_mkfile };
    vec![("Enter", what), ("Esc", t.cancel)]
}

/// Форма создания директории или файла: где создаём и строка ввода.
fn draw_create(
    frame: &mut Frame,
    area: Rect,
    form: &crate::ui::CreateForm,
    theme: &Theme,
    t: &'static Text,
) {
    let title = if form.is_dir {
        t.mkdir_title
    } else {
        t.mkfile_title
    };
    let place = format!("{} {}", t.create_in, form.dir.display());
    let hints = create_hints(form.is_dir, t);
    draw_name_input(frame, area, title, &place, &form.input, &hints, theme);
}

/// Окно со строкой ввода имени: пояснение сверху, строка с курсором, подсказки.
/// Длинное имя прокручивается так, чтобы курсор оставался виден.
fn draw_name_input(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    label: &str,
    input: &crate::ui::NameInput,
    hints: &[(&'static str, &'static str)],
    theme: &Theme,
) {
    let inner = dialog_inner(area);
    let chars: Vec<char> = input.name.chars().collect();
    // Сколько символов уходит за левый край, чтобы курсор влез в строку.
    let room = inner.saturating_sub(1).max(1);
    let mut start = 0;
    while chars[start..input.cursor]
        .iter()
        .collect::<String>()
        .width()
        >= room
    {
        start += 1;
    }
    let before: String = chars[start..input.cursor].iter().collect();
    let under: String = chars
        .get(input.cursor)
        .map_or(" ".to_string(), char::to_string);
    let after: String = chars.iter().skip(input.cursor + 1).collect();
    let after = fit(
        &after,
        "",
        room.saturating_sub(before.width() + under.width()),
    );
    let bold = Style::default().add_modifier(ratatui::style::Modifier::BOLD);
    let body = vec![
        Line::styled(fit(label, "", inner), theme.dim()),
        Line::raw(""),
        Line::from(vec![
            Span::styled(before, bold),
            Span::styled(under, theme.cursor(true)),
            Span::styled(after, bold),
        ]),
        Line::raw(""),
        hint_line(hints, inner, theme),
    ];
    dialog(frame, area, title, body, theme.border(true), theme);
}

/// Клавиши окна дисков.
fn drives_hints(t: &'static Text) -> Vec<(&'static str, &'static str)> {
    vec![
        ("↑↓", t.hint_move),
        ("Enter", t.hint_goto),
        ("Esc q", t.cancel),
    ]
}

/// Ширина полосы заполнения диска.
const DRIVE_BAR: usize = 10;

/// Окно выбора диска: имя, тип, полоса заполнения, свободное место, метка.
fn draw_drives(
    frame: &mut Frame,
    area: Rect,
    form: &crate::ui::DrivesForm,
    theme: &Theme,
    t: &'static Text,
) {
    use crate::drives::Kind as DriveKind;
    let name_width = form
        .drives
        .iter()
        .map(|d| d.name.width())
        .max()
        .unwrap_or(0)
        .min(16);
    let kind_label = |kind: DriveKind| match kind {
        DriveKind::Fixed => t.drive_fixed,
        DriveKind::Removable => t.drive_removable,
        DriveKind::Network => t.drive_network,
        DriveKind::Optical => t.drive_optical,
        DriveKind::Home => t.drive_home,
        DriveKind::Other => t.drive_other,
    };
    let kind_width = form
        .drives
        .iter()
        .map(|d| kind_label(d.kind).width())
        .max()
        .unwrap_or(0);
    // Рамка, отступ, пустая строка и подсказки — не больше, чем влезает.
    let rows = (area.height as usize).saturating_sub(5).max(1);
    let first = form.selected.saturating_sub(rows - 1);
    let mut body: Vec<Line> = form
        .drives
        .iter()
        .enumerate()
        .skip(first)
        .take(rows)
        .map(|(i, drive)| {
            let mut spans = vec![
                Span::styled(
                    format!("{:<name_width$}  ", fit(&drive.name, "", name_width)),
                    theme.dir(),
                ),
                Span::styled(
                    format!("{:<kind_width$}  ", kind_label(drive.kind)),
                    theme.dim(),
                ),
            ];
            match drive.space {
                Some(space) if space.total > 0 => {
                    let used = space.total.saturating_sub(space.free);
                    let filled = (used as u128 * DRIVE_BAR as u128 / space.total as u128) as usize;
                    let share = crate::ui::theme::heat_t(used, space.total);
                    spans.push(Span::styled("█".repeat(filled), theme.heat(share)));
                    spans.push(Span::styled("░".repeat(DRIVE_BAR - filled), theme.dim()));
                    spans.push(Span::raw(format!(
                        "  {:>8} {} {:>8}",
                        format_size(space.free),
                        t.free_of,
                        format_size(space.total)
                    )));
                }
                _ => spans.push(Span::styled(" ".repeat(DRIVE_BAR), theme.dim())),
            }
            if !drive.label.is_empty() {
                spans.push(Span::styled(format!("  {}", drive.label), theme.dim()));
            } else if drive.path.as_os_str() != drive.name.as_str() {
                // В Unix имя — последняя часть пути; полный путь — подсказкой.
                spans.push(Span::styled(
                    format!("  {}", display_path(&drive.path)),
                    theme.dim(),
                ));
            }
            let line = Line::from(spans);
            if i == form.selected {
                line.patch_style(theme.cursor(true))
            } else {
                line
            }
        })
        .collect();
    let inner = dialog_inner(area);
    body.push(Line::raw(""));
    body.push(hint_line(&drives_hints(t), inner, theme));
    dialog(frame, area, t.drives_title, body, theme.border(true), theme);
}

/// Ширина полосы доли в режиме «большие объекты».
const LARGE_BAR: usize = 12;

/// Режим «большие объекты» на весь экран: размер, полоса доли, процент, имя.
fn draw_large(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let t = app.text();
    let shown = match &app.large {
        Some(large) => app.panels[app.active as usize].display_path(&large.current_path()),
        None => return,
    };
    let Some(large) = app.large.as_mut() else {
        return;
    };
    let title = format!(
        " {}: {} ",
        t.large_title,
        shorten_path(
            &display_path(&shown),
            (area.width as usize).saturating_sub(t.large_title.width() + 6)
        )
    );
    let summary = match large.current() {
        Some(node) => format!(
            " {} · {} {} ",
            format_size(node.size),
            crate::ui::theme::format_count(node.items),
            t.objects
        ),
        None => {
            let (files, dirs, bytes) = large.progress.snapshot();
            format!(
                " {} {files} {} · {dirs} {} · {} · {:.1}s ",
                t.scanning,
                t.info_files,
                t.dirs_scanned,
                format_size(bytes),
                large.started.elapsed().as_secs_f32()
            )
        }
    };
    let counter = if large.order.is_empty() {
        String::new()
    } else {
        format!(" {}/{} ", large.cursor + 1, large.order.len())
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border(true))
        .title(Line::styled(title, theme.title(true)))
        .title_bottom(Line::styled(summary, theme.dim()).left_aligned())
        .title_bottom(Line::styled(counter, theme.dim()).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let rows = inner.height as usize;
    let width = inner.width as usize;
    large.fit(rows);
    let Some(node) = large.current() else {
        return;
    };
    // Шкала цвета — относительно крупнейшего на этом уровне, доля — от целого.
    let max = large
        .order
        .first()
        .map(|&i| node.children[i].size)
        .unwrap_or(0);
    let total = node.size.max(1);
    let lines: Vec<Line> = large
        .order
        .iter()
        .enumerate()
        .skip(large.offset)
        .take(rows)
        .map(|(row, &i)| {
            let child = &node.children[i];
            let heat = theme.heat(crate::ui::theme::heat_t(child.size, max));
            let share = child.size as f64 / total as f64;
            let filled = ((share * LARGE_BAR as f64).round() as usize)
                .clamp(usize::from(child.size > 0), LARGE_BAR);
            let (style, suffix) = match child.kind {
                crate::scan::Kind::Dir => (theme.dir(), "/"),
                crate::scan::Kind::Symlink => (theme.link(), ""),
                _ => (Style::default(), ""),
            };
            let mut name = format!("{}{suffix}", child.display_name());
            if child.denied {
                name.push_str(&format!("  ({})", t.denied));
            }
            let head = format!(" {:>9}  ", format_size(child.size));
            let tail = format!(" {:>5.1}%  ", share * 100.0);
            let room = width.saturating_sub(head.width() + LARGE_BAR + tail.width());
            let name = fit(&name, "", room);
            let pad = room.saturating_sub(name.width());
            let line = Line::from(vec![
                Span::styled(head, heat),
                Span::styled("█".repeat(filled), heat),
                Span::styled("░".repeat(LARGE_BAR - filled), theme.dim()),
                Span::styled(tail, theme.dim()),
                Span::styled(name, style),
                Span::raw(" ".repeat(pad)),
            ]);
            if row == large.cursor {
                line.patch_style(theme.cursor(true))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Строка запроса поиска в панели: вид поиска, запрос, клавиши.
fn finder_line<'a>(finder: &Finder, width: usize, theme: &Theme, t: &'a Text) -> Line<'a> {
    let (key, label) = match finder.kind {
        SearchKind::Content => ("/", t.search_in_files),
        SearchKind::Name => ("f", t.search_by_name),
    };
    let mut spans = vec![
        Span::styled(format!("{key} "), theme.key()),
        Span::styled(format!("{label}: "), theme.dim()),
        Span::raw(finder.query.clone()),
        Span::styled("▏", theme.key()),
        Span::raw("   "),
    ];
    let used: usize = spans.iter().map(Span::width).sum();
    let keys = hint_line(
        &[
            ("↑↓", t.hint_results),
            ("Enter", t.hint_goto),
            ("Esc", t.hint_exit),
        ],
        width.saturating_sub(used),
        theme,
    );
    spans.extend(keys.spans);
    Line::from(spans)
}

/// Список находок на месте файловой панели.
fn draw_finder(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let t = app.text();
    let root = match &app.finder {
        Some(f) => app.panels[f.side as usize].display_path(&f.root),
        None => return,
    };
    let Some(finder) = app.finder.as_mut() else {
        return;
    };
    let label = match finder.kind {
        SearchKind::Content => t.search_in_files,
        SearchKind::Name => t.search_by_name,
    };
    let title_width = (area.width as usize).saturating_sub(label.width() + 8);
    let title = format!(
        " {label}: {} ",
        shorten_path(&display_path(&root), title_width)
    );
    let counter = if finder.hits.is_empty() {
        String::new()
    } else {
        format!(" {}/{} ", finder.cursor + 1, finder.hits.len())
    };
    let mut state = format!("{} {}", finder.files, t.scanned);
    if finder.running {
        state = format!("{} {state}", t.searching);
    }
    if finder.limited {
        state = format!("{state} · {}", t.limit_reached);
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border(true))
        .title(Line::styled(title, theme.title(true)))
        .title_bottom(Line::styled(format!(" {state} "), theme.dim()).left_aligned())
        .title_bottom(Line::styled(counter, theme.dim()).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let rows = inner.height as usize;
    let width = inner.width as usize;

    if finder.hits.is_empty() {
        let note = if finder.query.is_empty() {
            t.type_to_search
        } else if finder.running {
            t.searching
        } else {
            t.no_matches
        };
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {note}"), theme.dim())),
            inner,
        );
        return;
    }

    finder.fit(rows);
    let lines: Vec<Line> = finder
        .hits
        .iter()
        .enumerate()
        .skip(finder.offset)
        .take(rows)
        .map(|(i, hit)| {
            // Путь приглушён, найденное подсвечено, как в просмотре.
            let mut cells: Vec<Cell> = vec![(' ', Style::default())];
            let push = |cells: &mut Vec<Cell>, text: &str, style: Style| {
                cells.extend(text.chars().map(|c| (c, style)));
            };
            let found = |cells: &mut Vec<Cell>, text: &str, at: (usize, usize), base: Style| {
                for (k, ch) in text.chars().enumerate() {
                    let hit = k >= at.0 && k < at.0 + at.1;
                    cells.push((
                        ch,
                        if hit {
                            base.patch(theme.found(true))
                        } else {
                            base
                        },
                    ));
                }
            };
            match hit.line {
                Some(line) => {
                    push(&mut cells, &finder.relative(hit), theme.link());
                    push(&mut cells, &format!(":{}: ", line + 1), theme.dim());
                    found(&mut cells, &hit.text, hit.at, Style::default());
                }
                None => {
                    let rel = finder.relative(hit);
                    let parent = rel.strip_suffix(&hit.text).unwrap_or("");
                    push(&mut cells, parent, theme.dim());
                    let base = if hit.is_dir {
                        theme.dir()
                    } else {
                        Style::default()
                    };
                    found(&mut cells, &hit.text, hit.at, base);
                    if hit.is_dir {
                        push(&mut cells, "/", theme.dir());
                    }
                }
            }
            let cells = slice_cells(&cells, 0, width);
            let pad = width.saturating_sub(cells.iter().map(|c| c.0.width().unwrap_or(0)).sum());
            let mut spans = group_cells(cells);
            spans.push(Span::raw(" ".repeat(pad)));
            let line = Line::from(spans);
            if i == finder.cursor {
                line.patch_style(theme.cursor(true))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Строка ввода поиска вместо подсказок: запрос, счётчик, клавиши.
fn search_line<'a>(search: &Search, width: usize, theme: &Theme, t: &'a Text) -> Line<'a> {
    let counter = if search.query.is_empty() {
        String::new()
    } else if search.matches.is_empty() {
        t.no_matches.to_string()
    } else {
        format!("{}/{}", search.current + 1, search.matches.len())
    };
    let mut spans = vec![
        Span::styled("/", theme.key()),
        Span::raw(search.query.clone()),
        Span::styled("▏", theme.key()),
        Span::raw("  "),
        Span::styled(counter, theme.dim()),
        Span::raw("   "),
    ];
    let used: usize = spans.iter().map(Span::width).sum();
    let keys = hint_line(
        &[("←→", t.hint_prev_next), ("Esc", t.hint_exit)],
        width.saturating_sub(used),
        theme,
    );
    spans.extend(keys.spans);
    Line::from(spans)
}

/// Наибольшая ширина диалога: шире длинные пути читать неудобно.
const DIALOG_MAX_WIDTH: u16 = 72;

/// Рисует диалог по центру `area`. Ширина подбирается под содержимое.
fn dialog(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    body: Vec<Line>,
    border: Style,
    theme: &Theme,
) {
    // Рамка и поля по одному символу с каждой стороны.
    let content = body.iter().map(Line::width).max().unwrap_or(0) as u16;
    let width = (content + 4)
        .max(title.width() as u16 + 6)
        .clamp(30, DIALOG_MAX_WIDTH)
        .min(area.width);
    let height = (body.len() as u16 + 2).min(area.height);
    let rect = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(Line::styled(format!(" {title} "), border))
        .padding(Padding::horizontal(1))
        .style(theme.base());
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(body).block(block), rect);
}

/// Ширина текста внутри диалога наибольшей ширины.
fn dialog_inner(area: Rect) -> usize {
    (DIALOG_MAX_WIDTH.min(area.width) as usize).saturating_sub(4)
}

fn draw_confirm(
    frame: &mut Frame,
    area: Rect,
    permanent: bool,
    targets: &[std::path::PathBuf],
    theme: &Theme,
    t: &Text,
) {
    let inner = dialog_inner(area);
    let (title, question, border) = if permanent {
        (t.delete_forever_title, t.delete_forever, theme.error())
    } else {
        (t.delete_title, t.delete_to_trash, theme.border(true))
    };
    let what = match targets {
        [one] => shorten_path(&display_path(one), inner),
        many => format!("{}: {}", t.items, many.len()),
    };
    let key = if permanent { "D" } else { "d" };
    let body = vec![
        Line::raw(question),
        Line::raw(""),
        Line::styled(
            what,
            Style::default().add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Line::raw(""),
        hint_line(&[(key, t.confirm), ("Esc", t.cancel)], inner, theme),
    ];
    dialog(frame, area, title, body, border, theme);
}

fn draw_job(frame: &mut Frame, area: Rect, job: &Job, theme: &Theme, t: &'static Text) {
    let inner = dialog_inner(area);
    let title = match job.kind {
        JobKind::Copy => t.job_copy,
        JobKind::Move => t.job_move,
        JobKind::Trash => t.job_trash,
        JobKind::Delete => t.job_delete,
        JobKind::Extract => t.job_extract,
        JobKind::Pack => t.job_pack,
    };
    let p = &job.progress;
    let current = shorten_path(&display_path(&p.current), inner);
    let footer = if job.cancelling {
        Line::styled(t.cancelling, theme.dim())
    } else {
        hint_line(&[("Esc", t.cancel)], inner, theme)
    };
    // Вопрос о конфликте заменяет окно прогресса: два окна друг на друге
    // читаются хуже, а прогресс, пока операция ждёт ответа, всё равно стоит.
    if let Some((path, _)) = &job.conflict {
        let mut body = vec![
            Line::styled(
                shorten_path(&display_path(path), inner),
                Style::default().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Line::raw(""),
        ];
        for row in conflict_hints(t) {
            body.push(hint_line(&row, inner, theme));
        }
        dialog(frame, area, t.conflict_exists, body, theme.error(), theme);
        return;
    }
    let body = vec![
        Line::styled(current, theme.dim()),
        gauge(p, inner, theme),
        Line::styled(counters(p), theme.dim()),
        Line::raw(""),
        footer,
    ];
    dialog(frame, area, title, body, theme.border(true), theme);
}

/// Полоса прогресса с процентом. Доля считается по байтам, а если байтов
/// нет (пустые файлы, удаление) — по числу элементов.
fn gauge<'a>(p: &Progress, width: usize, theme: &Theme) -> Line<'a> {
    let ratio = if p.bytes_total > 0 {
        p.bytes_done as f64 / p.bytes_total as f64
    } else if p.items_total > 0 {
        p.items_done as f64 / p.items_total as f64
    } else {
        0.0
    };
    let ratio = ratio.clamp(0.0, 1.0);
    let bar = width.saturating_sub(5);
    let filled = ((bar as f64) * ratio).round() as usize;
    Line::from(vec![
        Span::styled("█".repeat(filled), theme.border(true)),
        Span::styled("░".repeat(bar - filled), theme.dim()),
        Span::raw(format!(" {:>3}%", (ratio * 100.0).round() as u32)),
    ])
}

/// Счётчики операции: «12/40 · 1.5 M/20.0 M».
fn counters(p: &Progress) -> String {
    let mut out = format!("{}/{}", p.items_done, p.items_total);
    if p.bytes_total > 0 {
        out.push_str(&format!(
            " · {}/{}",
            format_size(p.bytes_done),
            format_size(p.bytes_total)
        ));
    }
    out
}

/// Собирает строку подсказок; то, что не влезает, заменяется многоточием.
fn hint_line<'a>(hints: &[(&'a str, &'a str)], width: usize, theme: &Theme) -> Line<'a> {
    let mut spans = Vec::new();
    let mut used = 0;
    for (i, (key, desc)) in hints.iter().enumerate() {
        let item = key.width() + 1 + desc.width() + 2;
        // Последний элемент не обязан оставлять место под многоточие.
        let reserve = if i + 1 == hints.len() { 0 } else { 1 };
        if used + item + reserve > width {
            if used < width {
                spans.push(Span::styled("…", theme.dim()));
            }
            break;
        }
        spans.push(Span::styled(*key, theme.key()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*desc, theme.dim()));
        spans.push(Span::raw("  "));
        used += item;
    }
    Line::from(spans)
}

/// Диалог сохранения из редактора.
fn draw_save(frame: &mut Frame, area: Rect, app: &App) {
    let theme = app.theme;
    let t = app.text();
    let inner = dialog_inner(area);
    let name = app
        .editor
        .as_ref()
        .map(|e| shorten_path(&display_path(&e.path), inner))
        .unwrap_or_default();
    let (title, question, keys): (&str, &str, Vec<(&str, &str)>) = match app.mode {
        Mode::EditorExit => (
            t.unsaved_title,
            t.unsaved_question,
            vec![("y", t.save), ("n", t.discard), ("Esc", t.keep_editing)],
        ),
        _ => (
            t.save_title,
            t.save_question,
            vec![("y", t.save), ("Esc", t.cancel)],
        ),
    };
    let body = vec![
        Line::raw(question),
        Line::raw(""),
        Line::styled(
            name,
            Style::default().add_modifier(ratatui::style::Modifier::BOLD),
        ),
        Line::raw(""),
        hint_line(&keys, inner, &theme),
    ];
    dialog(frame, area, title, body, theme.border(true), &theme);
}

/// Редактор на месте просмотра: номера строк, подсветка, курсор терминала.
fn draw_editor(frame: &mut Frame, area: Rect, app: &mut App, side: Side) {
    let active = app.active == side;
    let theme = app.theme;
    let t = app.text();
    let Some(editor) = app.editor.as_mut() else {
        return;
    };

    let title_width = (area.width as usize).saturating_sub(8);
    let mark = if editor.modified { " ●" } else { "" };
    let title = format!(
        " ✎ {}{mark} ",
        shorten_path(&display_path(&editor.path), title_width)
    );
    let indent = &editor.indent;
    let kind = if indent.indent.tabs {
        t.tabs.to_string()
    } else {
        format!("{} {}", t.spaces, indent.indent.size)
    };
    let source = match indent.source {
        crate::indent::Source::EditorConfig => t.from_editorconfig,
        crate::indent::Source::Detected => t.from_file,
        crate::indent::Source::Language => t.from_language,
        crate::indent::Source::Default => t.from_settings,
    };
    let mut info = vec![
        format!(
            "{} {}, {} {}",
            t.line_short,
            editor.row + 1,
            t.col_short,
            editor.screen_col(editor.row, editor.col) + 1
        ),
        format!("{kind} ({source})"),
        editor.eol_name().to_string(),
    ];
    if let Some(language) = editor.language {
        info.push(language.name.to_string());
    }
    // Подпись набирается по важности: что не влезает в рамку, отбрасывается.
    let room = (area.width as usize).saturating_sub(4);
    let mut shown = String::new();
    for part in info {
        let next = if shown.is_empty() {
            part
        } else {
            format!("{shown} · {part}")
        };
        if next.width() + 2 > room {
            break;
        }
        shown = next;
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border(active))
        .title(Line::styled(title, theme.title(active)))
        .title_bottom(Line::styled(format!(" {shown} "), theme.dim()).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let rows = inner.height as usize;
    let gutter = editor.lines.len().to_string().len() + 1;
    let avail = (inner.width as usize).saturating_sub(gutter);
    editor.fit(rows, avail);

    let mut lines = Vec::with_capacity(rows);
    let last = (editor.top + rows).min(editor.lines.len());
    for row in editor.top..last {
        let pieces = editor.highlighted(row).to_vec();
        // Табуляция раскрывается в пробелы до следующей ступени.
        let mut cells: Vec<Cell> = Vec::new();
        let mut x = 0;
        for piece in &pieces {
            let style = theme.token(piece.kind);
            for ch in piece.text.chars() {
                let w = editor.char_width(ch, x);
                if ch == '\t' {
                    cells.extend(std::iter::repeat_n((' ', style), w));
                } else {
                    cells.push((ch, style));
                }
                x += w;
            }
        }
        let number_style = if row == editor.row {
            theme.title(true)
        } else {
            theme.dim()
        };
        let mut spans = vec![Span::styled(
            format!("{:>w$} ", row + 1, w = gutter - 1),
            number_style,
        )];
        spans.extend(group_cells(slice_cells(&cells, editor.left, avail)));
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines), inner);

    // Настоящий курсор терминала: его видно и в терминалах с особым курсором.
    if active && matches!(app.mode, Mode::Normal) {
        let editor = app.editor.as_ref().unwrap();
        let x = editor
            .screen_col(editor.row, editor.col)
            .saturating_sub(editor.left)
            + gutter;
        let y = editor.row - editor.top;
        if x < inner.width as usize && y < rows {
            frame.set_cursor_position((inner.x + x as u16, inner.y + y as u16));
        }
    }
}

fn draw_viewer(frame: &mut Frame, area: Rect, app: &mut App, side: Side) {
    let active = app.active == side;
    let theme = app.theme;
    let t = app.text();
    let Some(viewer) = app.viewer.as_mut() else {
        return;
    };

    let title_width = (area.width as usize).saturating_sub(4);
    let shown = viewer
        .path
        .as_ref()
        .map(|p| app.panels[side.other() as usize].display_path(p));
    let Some(viewer) = app.viewer.as_mut() else {
        return;
    };
    let title = shown
        .map(|p| shorten_path(&display_path(&p), title_width))
        .unwrap_or_default();
    let mut block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.border(active))
        .title(Line::styled(format!(" {title} "), theme.title(active)))
        .title_bottom(Line::styled(viewer_info(viewer, t), theme.dim()).right_aligned());
    if let Some(search) = viewer.search.as_ref().filter(|s| !s.query.is_empty()) {
        let label = match search.matches.len() {
            0 => format!(" {} ", t.no_matches),
            n => format!(" {}/{n} ", search.current + 1),
        };
        block = block.title_bottom(Line::styled(label, theme.key()).left_aligned());
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let rows = inner.height as usize;
    let width = inner.width as usize;

    // Просмотру нечего показывать (поиск ещё ничего не нашёл) — он пуст.
    if viewer.path.is_none() {
        viewer.fit(rows, 0);
        return;
    }
    let Some(content) = &viewer.content else {
        viewer.fit(rows, 0);
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {}", t.loading), theme.dim())),
            inner,
        );
        return;
    };

    // Сначала предел прокрутки — от него зависит, с какой строки рисовать.
    let lines = match &content.body {
        Body::Text(text) => {
            let max_top = text_max_top(text, rows, text_avail(text, width), viewer.wrap);
            viewer.fit(rows, max_top);
            text_rows(viewer, rows, width, &theme, t)
        }
        body => {
            let lines = match body {
                Body::Dir(info) => dir_lines(content, info, width, &theme, t),
                Body::Binary(dump) => binary_lines(dump, &theme, t),
                Body::Archive(items) => archive_lines(items, &theme),
                Body::Error(err) => vec![Line::styled(
                    format!("{}: {err}", t.cannot_read),
                    theme.error(),
                )],
                Body::Text(_) => unreachable!(),
            };
            viewer.fit(rows, lines.len().saturating_sub(rows));
            let left = if viewer.wrap { 0 } else { viewer.left };
            lines
                .into_iter()
                .skip(viewer.top)
                .take(rows)
                .map(|line| shift_line(line, left))
                .collect()
        }
    };
    frame.render_widget(Paragraph::new(lines), inner);
}

/// Подпись в нижней рамке просмотра: вид, размер, число строк, режимы.
fn viewer_info(viewer: &Viewer, t: &Text) -> String {
    let Some(content) = &viewer.content else {
        return String::new();
    };
    let parts: Vec<String> = match &content.body {
        Body::Text(text) => {
            let mut parts = vec![
                text.language.unwrap_or("text").to_string(),
                format_size(content.size),
                format!("{} {}", text.lines.len(), t.lines),
            ];
            if text.truncated {
                parts.push(t.truncated.to_string());
            }
            if viewer.wrap {
                parts.push(t.wrap.to_string());
            }
            parts
        }
        Body::Dir(_) => vec![t.info_directory.to_string()],
        Body::Binary(_) => vec![t.binary.to_string(), format_size(content.size)],
        Body::Archive(items) => vec![
            t.archive.to_string(),
            format!("{} {}", items.len(), t.entries),
            format_size(content.size),
        ],
        Body::Error(_) => return String::new(),
    };
    format!(" {} ", parts.join(" · "))
}

/// Ширина колонки номеров строк вместе с пробелом после них.
fn gutter_width(text: &TextBody) -> usize {
    if text.numbered {
        text.lines.len().max(1).to_string().len() + 1
    } else {
        0
    }
}

/// Ширина под текст строки рядом с номерами.
fn text_avail(text: &TextBody, width: usize) -> usize {
    width.saturating_sub(gutter_width(text))
}

/// Сколько экранных строк займёт строка шириной `w` при переносе.
fn wrapped_rows(w: usize, avail: usize) -> usize {
    if avail == 0 {
        1
    } else {
        w.div_ceil(avail).max(1)
    }
}

/// Наибольшая первая строка, при которой конец файла упирается в низ экрана.
fn text_max_top(text: &TextBody, rows: usize, avail: usize, wrap: bool) -> usize {
    let n = text.lines.len();
    if !wrap {
        return n.saturating_sub(rows);
    }
    let mut used = 0;
    let mut first = n;
    while first > 0 {
        let h = wrapped_rows(text.widths[first - 1], avail);
        if used + h > rows {
            break;
        }
        used += h;
        first -= 1;
    }
    first
}

/// Видимые строки текста с номерами, переносом и отметками поиска.
fn text_rows<'a>(
    viewer: &Viewer,
    rows: usize,
    width: usize,
    theme: &Theme,
    t: &Text,
) -> Vec<Line<'a>> {
    let Some(Body::Text(text)) = viewer.content.as_ref().map(|c| &c.body) else {
        return Vec::new();
    };
    if text.lines.is_empty() {
        return vec![Line::styled(t.empty_file.to_string(), theme.dim())];
    }
    let gutter = gutter_width(text);
    let avail = width.saturating_sub(gutter);
    let mut out = Vec::with_capacity(rows);
    let mut i = viewer.top;
    while out.len() < rows && i < text.lines.len() {
        let cells = styled_cells(&text.lines[i], i, viewer.search.as_ref(), theme);
        let chunks = if viewer.wrap {
            wrap_cells(&cells, avail)
        } else {
            vec![slice_cells(&cells, viewer.left, avail)]
        };
        for (k, chunk) in chunks.into_iter().enumerate() {
            if out.len() >= rows {
                break;
            }
            let mut spans = Vec::new();
            if gutter > 0 {
                let number = if k == 0 {
                    format!("{:>w$} ", i + 1, w = gutter - 1)
                } else {
                    " ".repeat(gutter)
                };
                spans.push(Span::styled(number, theme.dim()));
            }
            spans.extend(group_cells(chunk));
            out.push(Line::from(spans));
        }
        i += 1;
    }
    out
}

/// Символ строки и его стиль.
type Cell = (char, Style);

/// Раскладывает строку на символы со стилями подсветки и найденного текста.
fn styled_cells(
    pieces: &[Piece],
    line: usize,
    search: Option<&Search>,
    theme: &Theme,
) -> Vec<Cell> {
    let mut cells: Vec<Cell> = pieces
        .iter()
        .flat_map(|p| {
            let style = theme.token(p.kind);
            p.text.chars().map(move |c| (c, style))
        })
        .collect();
    if let Some(search) = search {
        let current = search.current();
        for m in search.in_line(line) {
            let style = theme.found(current == Some(*m));
            for cell in cells.iter_mut().skip(m.start).take(m.len) {
                cell.1 = cell.1.patch(style);
            }
        }
    }
    cells
}

/// Режет строку на куски шириной не больше `avail`. Широкий символ, не
/// влезающий в остаток, переносится целиком.
fn wrap_cells(cells: &[Cell], avail: usize) -> Vec<Vec<Cell>> {
    let mut rows = vec![Vec::new()];
    if avail == 0 {
        return rows;
    }
    let mut used = 0;
    for &cell in cells {
        let w = cell.0.width().unwrap_or(0);
        if used + w > avail && used > 0 {
            rows.push(Vec::new());
            used = 0;
        }
        rows.last_mut().unwrap().push(cell);
        used += w;
    }
    rows
}

/// Видимый кусок строки при горизонтальной прокрутке на `left` колонок.
fn slice_cells(cells: &[Cell], left: usize, avail: usize) -> Vec<Cell> {
    let mut out = Vec::new();
    let mut col = 0;
    let mut used = 0;
    for &cell in cells {
        let w = cell.0.width().unwrap_or(0);
        if col < left {
            // Широкий символ, разрезанный краем, заменяется пробелом.
            if col + w > left {
                out.push((' ', cell.1));
                used += 1;
            }
            col += w;
            continue;
        }
        if used + w > avail {
            break;
        }
        out.push(cell);
        used += w;
        col += w;
    }
    out
}

/// Склеивает соседние символы одного стиля в фрагменты.
fn group_cells<'a>(cells: Vec<Cell>) -> Vec<Span<'a>> {
    let mut spans: Vec<Span> = Vec::new();
    let mut text = String::new();
    let mut style = None;
    for (ch, st) in cells {
        if style.is_some_and(|s| s != st) {
            spans.push(Span::styled(std::mem::take(&mut text), style.unwrap()));
        }
        style = Some(st);
        text.push(ch);
    }
    if let Some(style) = style {
        spans.push(Span::styled(text, style));
    }
    spans
}

/// Сдвигает готовую строку влево на `left` колонок.
fn shift_line(line: Line<'_>, left: usize) -> Line<'_> {
    if left == 0 {
        return line;
    }
    let cells: Vec<Cell> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
        .collect();
    Line::from(group_cells(slice_cells(&cells, left, usize::MAX)))
}

/// Сводка о директории.
fn dir_lines<'a>(
    content: &Preview,
    info: &DirInfo,
    width: usize,
    theme: &Theme,
    t: &Text,
) -> Vec<Line<'a>> {
    let name = content
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| content.path.display().to_string());
    let labels = [
        t.info_contents,
        t.info_size,
        t.info_modified,
        t.info_permissions,
    ];
    let pad = labels.iter().map(|l| l.width()).max().unwrap_or(0) + 2;
    let row = |label: &str, value: String| {
        Line::from(vec![
            Span::styled(format!("{label:<pad$}", pad = pad), theme.dim()),
            Span::raw(value),
        ])
    };

    let contents = format!(
        "{} {} · {} {} · {} {} · {} {}",
        info.files,
        t.info_files,
        info.dirs,
        t.info_dirs,
        info.links,
        t.info_links,
        info.hidden,
        t.info_hidden
    );
    let size = if info.complete {
        format!(
            "{} · {} {}",
            format_size(info.total_size),
            info.total_items,
            t.info_in_items
        )
    } else {
        format!(
            "{} {} · {} {}",
            t.info_partial,
            format_size(info.total_size),
            info.total_items,
            t.info_in_items
        )
    };
    let modified = content.modified.map(format_time).unwrap_or_default();
    let mut lines = vec![
        Line::styled(format!("{name}/"), theme.dir()),
        Line::raw(""),
        row(t.info_contents, contents),
        row(t.info_size, size),
        row(t.info_modified, modified),
        row(
            t.info_permissions,
            format!("{} {}", content.mode, content.owner),
        ),
    ];
    if info.largest.is_empty() {
        return lines;
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(t.info_largest.to_string(), theme.dim()));
    // Полоса относительно самого крупного; размер справа от неё.
    let top = info.largest[0].1.max(1);
    let bar = 10.min(width / 4);
    for (name, size, is_dir) in &info.largest {
        let filled = ((*size as f64 / top as f64) * bar as f64).round() as usize;
        let style = if *is_dir {
            theme.dir()
        } else {
            Style::default()
        };
        let suffix = if *is_dir { "/" } else { "" };
        lines.push(Line::from(vec![
            Span::styled("█".repeat(filled), theme.border(true)),
            Span::styled("░".repeat(bar - filled), theme.dim()),
            Span::styled(format!(" {:>8} ", format_size(*size)), theme.dim()),
            Span::styled(format!("{name}{suffix}"), style),
        ]));
    }
    lines
}

/// Содержимое архива: размер и путь, директории выделены.
fn archive_lines<'a>(items: &[crate::archive::Item], theme: &Theme) -> Vec<Line<'a>> {
    items
        .iter()
        .map(|item| {
            let size = if item.is_dir {
                String::new()
            } else {
                format_size(item.size)
            };
            let (style, suffix) = if item.is_dir {
                (theme.dir(), "/")
            } else {
                (Style::default(), "")
            };
            Line::from(vec![
                Span::styled(format!("{size:>9}  "), theme.dim()),
                Span::styled(format!("{}{suffix}", item.name), style),
            ])
        })
        .collect()
}

/// Клавиши формы архива.
fn pack_hints(t: &'static Text) -> Vec<(&'static str, &'static str)> {
    vec![
        ("←→", t.hint_format),
        ("Enter", t.create),
        ("Esc", t.cancel),
    ]
}

/// Форма создания архива: строка форматов и поле имени.
fn draw_pack(
    frame: &mut Frame,
    area: Rect,
    form: &crate::ui::PackForm,
    theme: &Theme,
    t: &'static Text,
) {
    let inner = dialog_inner(area);
    let mut formats = vec![Span::styled(format!("{}: ", t.pack_format), theme.dim())];
    for (i, format) in crate::archive::Format::PACKABLE.iter().enumerate() {
        let label = format!(" {} ", format.extension());
        formats.push(if i == form.format {
            Span::styled(label, theme.cursor(true))
        } else {
            Span::raw(label)
        });
        formats.push(Span::raw(" "));
    }
    let name = fit(
        &form.file_name(),
        "",
        inner.saturating_sub(t.pack_name.width() + 3),
    );
    let what = match form.sources.as_slice() {
        [one] => one
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        many => format!("{}: {}", t.items, many.len()),
    };
    let body = vec![
        Line::styled(what, theme.dim()),
        Line::raw(""),
        Line::from(formats),
        Line::from(vec![
            Span::styled(format!("{}: ", t.pack_name), theme.dim()),
            Span::styled(
                name,
                Style::default().add_modifier(ratatui::style::Modifier::BOLD),
            ),
            Span::styled("▏", theme.key()),
        ]),
        Line::raw(""),
        hint_line(&pack_hints(t), inner, theme),
    ];
    dialog(frame, area, t.pack_title, body, theme.border(true), theme);
}

/// Hex-дамп: смещение приглушено, байты и символы обычным цветом.
fn binary_lines<'a>(dump: &[String], theme: &Theme, t: &Text) -> Vec<Line<'a>> {
    if dump.is_empty() {
        return vec![Line::styled(t.binary.to_string(), theme.dim())];
    }
    dump.iter()
        .map(|line| {
            let (offset, rest) = line.split_at(line.len().min(8));
            Line::from(vec![
                Span::styled(offset.to_string(), theme.dim()),
                Span::raw(rest.to_string()),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_keeps_short_names() {
        assert_eq!(fit("src", "/", 10), "src/");
        assert_eq!(fit("main.rs", "", 7), "main.rs");
    }

    #[test]
    fn fit_truncates_with_ellipsis_and_keeps_suffix() {
        assert_eq!(fit("verylongname", "/", 8), "verylo…/");
        assert_eq!(fit("verylongname", "", 5), "very…");
        assert_eq!(fit("abc", "/", 1), "…");
        assert_eq!(fit("abc", "", 0), "");
    }

    #[test]
    fn fit_respects_wide_characters() {
        // Иероглифы занимают по две колонки.
        let s = fit("日本語のファイル", "", 7);
        assert!(s.width() <= 7, "{s}");
        assert!(s.ends_with('…'));
    }

    #[test]
    fn shorten_path_keeps_the_tail() {
        assert_eq!(shorten_path("/a/b", 10), "/a/b");
        assert_eq!(shorten_path("/home/user/projects/fm", 10), "…ojects/fm");
        assert_eq!(shorten_path("/x", 0), "");
    }

    #[test]
    fn format_size_is_compact() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 K");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 G");
        // Самое длинное значение умещается в колонку размера.
        assert!(format_size(1024 * 1024 - 1).len() <= SIZE_WIDTH);
    }

    #[test]
    fn entry_line_has_exact_width() {
        let theme = Theme {
            depth: crate::ui::theme::ColorDepth::True,
            name: crate::ui::theme::ThemeName::Dark,
        };
        let entry = Entry {
            name: "Cargo.toml".into(),
            path: "/x/Cargo.toml".into(),
            kind: Kind::File,
            exec: false,
            size: 2048,
            modified: None,
            target: None,
        };
        for width in [0, 5, 20, 40, 80] {
            for details in [false, true] {
                let look = Look {
                    details,
                    cursor: Some(true),
                    marked: details,
                    vcs_dirty: false,
                };
                let line = entry_line(&entry, width, look, &theme, crate::i18n::Lang::En.text());
                assert_eq!(
                    line.width(),
                    width.max(1),
                    "width {width}, details {details}"
                );
            }
        }
    }

    #[test]
    fn hint_line_fits_width() {
        let theme = Theme {
            depth: crate::ui::theme::ColorDepth::True,
            name: crate::ui::theme::ThemeName::Dark,
        };
        let hints = [("a", "alpha"), ("b", "beta"), ("c", "gamma")];
        for width in 0..40 {
            assert!(hint_line(&hints, width, &theme).width() <= width);
        }
    }

    /// Текст кадра построчно — для проверок содержимого экрана.
    fn screen(app: &mut App, width: u16, height: u16) -> Vec<String> {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    fn sample_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        for i in 0..30 {
            std::fs::write(tmp.path().join(format!("file{i:02}.txt")), "").unwrap();
        }
        tmp
    }

    #[test]
    fn frame_shows_both_panels_and_hints() {
        let tmp = sample_dir();
        let mut app = App::new_for_test(tmp.path());
        let lines = screen(&mut app, 100, 12);
        println!("{}", lines.join("\n"));
        assert!(lines[1].matches("src/").count() == 2, "{}", lines[1]);
        assert!(lines[1].contains("<DIR>"));
        // Подсказки в две строки, и видно всё доступное — вплоть до выхода.
        let hints = format!("{}{}", lines[10], lines[11]);
        assert!(hints.contains("? help"), "{hints}");
        assert!(hints.contains("q quit"), "{hints}");
        // Обе панели в одной директории — копировать некуда, подсказки нет.
        assert!(!hints.contains("c copy"), "{hints}");
        // Счётчик позиции в нижней рамке.
        assert!(lines[9].contains("1/32"));
    }

    #[test]
    fn frame_with_columns_and_hidden_panel() {
        let tmp = sample_dir();
        let mut app = App::new_for_test(tmp.path());
        app.panel_mut(Side::Left).set_columns(3);
        app.hidden = Some(Side::Right);
        // Курсор за пределами первой страницы: 3 колонки по 8 строк.
        app.panel_mut(Side::Left).cursor = 30;
        let lines = screen(&mut app, 90, 11);
        println!("{}", lines.join("\n"));
        assert_eq!(lines[1].matches('│').count(), 4, "{}", lines[1]);
        assert!(lines.iter().any(|l| l.contains("file29.txt")));
        assert!(!lines.iter().any(|l| l.contains("Cargo.toml")));
    }

    #[test]
    fn frame_marks_and_delete_dialog() {
        let tmp = sample_dir();
        let mut app = App::new_for_test(tmp.path());
        let targets = app.panel(Side::Left).targets();
        app.panel_mut(Side::Left).toggle_mark();
        app.panel_mut(Side::Left).toggle_mark();
        app.mode = Mode::ConfirmDelete {
            permanent: true,
            targets,
        };
        let lines = screen(&mut app, 100, 16);
        println!("{}", lines.join("\n"));
        let all = lines.join("\n");
        assert!(all.contains("Delete permanently"));
        assert!(all.contains("cannot be undone"));
        assert!(lines[2].starts_with("│*Cargo.toml"), "{}", lines[2]);
        assert!(lines[15].contains("D confirm"));
        // Счётчик выделенного в нижней рамке: src/ и Cargo.toml (9 байт).
        assert!(lines[14].contains("marked: 2 · 9 B"), "{}", lines[14]);
    }

    #[test]
    fn frame_job_with_conflict() {
        let tmp = sample_dir();
        let mut app = App::new_for_test(tmp.path());
        let mut job = Job::delete(Vec::new(), false, Side::Left);
        job.progress = Progress {
            bytes_total: 200,
            bytes_done: 50,
            items_total: 4,
            items_done: 1,
            current: tmp.path().join("Cargo.toml"),
        };
        let (reply, _answers) = crossbeam_channel::bounded(1);
        job.conflict = Some((tmp.path().join("src"), reply));
        app.mode = Mode::Job(job);
        let lines = screen(&mut app, 100, 16);
        println!("{}", lines.join("\n"));
        let all = lines.join("\n");
        assert!(all.contains("Already exists"));
        assert!(all.contains("R replace all"));
        assert!(lines[15].contains("r replace"));

        // Без вопроса виден прогресс: доля по байтам и счётчики.
        if let Mode::Job(job) = &mut app.mode {
            job.conflict = None;
        }
        let lines = screen(&mut app, 100, 16);
        println!("{}", lines.join("\n"));
        let all = lines.join("\n");
        assert!(all.contains("Deleting"));
        assert!(all.contains(" 25%"));
        assert!(all.contains("1/4 · 50 B/200 B"));
    }

    fn viewer_app(name: &str, text: &str) -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(name), text).unwrap();
        let mut app = App::new_for_test(tmp.path());
        app.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Enter,
        ));
        crate::ui::tests::wait_preview(&mut app);
        (tmp, app)
    }

    #[test]
    fn frame_source_with_numbers_and_wrap() {
        let long = format!("let s = \"{}\";", "x".repeat(60));
        let text = format!("fn main() {{\n    {long}\n}}\n");
        let (_tmp, mut app) = viewer_app("main.rs", &text);
        let lines = screen(&mut app, 80, 10);
        println!("{}", lines.join("\n"));
        // Справа: номера строк, перенос длинной строки без номера.
        assert!(lines[1].contains("│1 fn main() {"), "{}", lines[1]);
        assert!(lines[2].contains("│2     let s"), "{}", lines[2]);
        assert!(lines[3].contains("│  xxx"), "{}", lines[3]);
        assert!(lines[8].contains("Rust"), "{}", lines[8]);
        assert!(lines[8].contains("3 lines"));
        assert!(lines[9].contains("search"));

        // Без переноса длинная строка обрезается.
        app.viewer.as_mut().unwrap().toggle_wrap();
        let lines = screen(&mut app, 80, 10);
        assert!(lines[3].contains("│3 }"), "{}", lines[3]);
    }

    #[test]
    fn frame_search_highlights_and_counts() {
        let text: String = (0..40).map(|i| format!("row {i}\n")).collect();
        let (_tmp, mut app) = viewer_app("data.txt", &text);
        let viewer = app.viewer.as_mut().unwrap();
        viewer.start_search();
        viewer.rows = 8;
        viewer.set_query("row 3".into());
        let lines = screen(&mut app, 80, 10);
        println!("{}", lines.join("\n"));
        // Первое совпадение — `row 3` в середине экрана; всего 11 (3, 30..39).
        assert!(lines.iter().any(|l| l.contains("1/11")));
        assert!(lines[9].starts_with(" /row 3"), "{}", lines[9]);
        // Простой текст — без номеров строк.
        assert!(lines.iter().any(|l| l.contains("│row 3 ")));
    }

    #[test]
    fn frame_directory_info() {
        let tmp = sample_dir();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn x() {}").unwrap();
        let mut app = App::new_for_test(tmp.path());
        app.on_key(ratatui::crossterm::event::KeyEvent::from(
            ratatui::crossterm::event::KeyCode::Char('p'),
        ));
        crate::ui::tests::wait_preview(&mut app);
        let lines = screen(&mut app, 110, 16);
        println!("{}", lines.join("\n"));
        let all = lines.join("\n");
        assert!(all.contains("src/"));
        assert!(all.contains("contents"));
        assert!(all.contains("largest"));
        assert!(all.contains("1 files · 0 dirs"));
        assert!(all.contains("lib.rs"));
        assert!(all.contains("directory"));
    }

    #[test]
    fn wrap_and_slice_respect_wide_chars() {
        let cells: Vec<Cell> = "ab日本c".chars().map(|c| (c, Style::default())).collect();
        let rows = wrap_cells(&cells, 3);
        let text: Vec<String> = rows
            .iter()
            .map(|r| r.iter().map(|c| c.0).collect())
            .collect();
        assert_eq!(text, ["ab", "日", "本c"]);
        let sliced: String = slice_cells(&cells, 3, 10).iter().map(|c| c.0).collect();
        assert_eq!(sliced, " 本c");
        assert_eq!(wrap_cells(&[], 5).len(), 1);
    }

    #[test]
    fn frame_editor_with_cursor_and_info() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.go"), "func main() {\n\tx := 1\n}\n").unwrap();
        let mut app = App::new_for_test(tmp.path());
        let key = |c| ratatui::crossterm::event::KeyEvent::from(c);
        use ratatui::crossterm::event::KeyCode;
        app.on_key(key(KeyCode::Enter));
        crate::ui::tests::wait_preview(&mut app);
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::End));

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 8)).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        let lines = screen(&mut app, 80, 8);
        println!("{}", lines.join("\n"));
        // Табуляция раскрыта на 4 колонки, язык Go — отступ табуляцией.
        assert!(lines[2].contains("│2     x := 1"), "{}", lines[2]);
        assert!(lines[6].contains("Ln 2, Col 11"), "{}", lines[6]);
        assert!(lines[6].contains("tabs (detected)"), "{}", lines[6]);
        assert!(
            !lines[6].contains("Go"),
            "язык не влез и отброшен: {}",
            lines[6]
        );
        assert!(lines[7].contains("Ctrl+S save"));
        // Курсор терминала после `1`: панель с x=40, рамка, номер (2 колонки), 10 колонок текста.
        terminal
            .backend_mut()
            .assert_cursor_position((40 + 1 + 2 + 10, 2));
    }

    #[test]
    fn frame_finder_with_preview() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        let code: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        std::fs::write(
            tmp.path().join("src/a.rs"),
            format!("{code}fn needle() {{}}\n"),
        )
        .unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "a needle here\n").unwrap();
        let mut app = App::new_for_test(tmp.path());
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        app.on_key(KeyEvent::from(KeyCode::Char('/')));
        for c in "needle".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(c)));
        }
        crate::ui::tests::wait_finder(&mut app);
        app.on_key(KeyEvent::from(KeyCode::Down));
        crate::ui::tests::wait_preview(&mut app);
        let lines = screen(&mut app, 100, 12);
        println!("{}", lines.join("\n"));
        let all = lines.join("\n");
        assert!(all.contains("notes.txt:1: a needle here"));
        assert!(all.contains("src/a.rs:31: fn needle() {}"));
        assert!(all.contains("2/2"));
        // Справа просмотр на найденной строке.
        assert!(all.contains("31 fn needle() {}"), "{all}");
        assert!(lines[11].contains("/ in files: needle"));
    }

    #[test]
    fn frame_pack_dialog_and_archive_preview() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("proj")).unwrap();
        std::fs::write(tmp.path().join("proj/a.txt"), "hello").unwrap();
        let mut app = App::new_for_test(tmp.path());
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        app.on_key(KeyEvent::from(KeyCode::Char('a')));
        app.on_key(KeyEvent::from(KeyCode::Right));
        app.on_key(KeyEvent::from(KeyCode::Right));
        let lines = screen(&mut app, 100, 14);
        println!("{}", lines.join("\n"));
        let all = lines.join("\n");
        assert!(all.contains("Create archive"));
        assert!(all.contains("name: proj.tar.gz"));
        assert!(lines[13].contains("←→ format"));

        app.on_key(KeyEvent::from(KeyCode::Enter));
        crate::ui::tests::finish(&mut app, KeyCode::Esc);
        assert!(tmp.path().join("proj.tar.gz").exists());

        // Просмотр архива — список его содержимого.
        app.on_key(KeyEvent::from(KeyCode::Down));
        app.on_key(KeyEvent::from(KeyCode::Char('p')));
        crate::ui::tests::wait_preview(&mut app);
        let lines = screen(&mut app, 100, 14);
        println!("{}", lines.join("\n"));
        let all = lines.join("\n");
        assert!(all.contains("proj/a.txt"), "{all}");
        assert!(all.contains("archive · 2 entries"), "{all}");
    }

    #[test]
    fn frame_large_objects() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("big")).unwrap();
        std::fs::write(tmp.path().join("big/blob"), vec![0u8; 3000]).unwrap();
        std::fs::write(tmp.path().join("small"), vec![0u8; 1000]).unwrap();
        let mut app = App::new_for_test(tmp.path());
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        app.on_key(KeyEvent::from(KeyCode::Char('l')));
        crate::ui::tests::wait_large(&mut app);
        let lines = screen(&mut app, 80, 8);
        println!("{}", lines.join("\n"));
        assert!(lines[0].contains("largest objects"));
        assert!(
            lines[1].contains("2.9 K") && lines[1].contains("75.0%") && lines[1].contains("big/"),
            "{}",
            lines[1]
        );
        assert!(lines[2].contains("small"));
        assert!(lines[6].contains("3.9 K · 3 objects"), "{}", lines[6]);
        assert!(lines[7].contains("Esc q exit"));
    }

    #[test]
    fn frame_help_and_settings() {
        let tmp = sample_dir();
        let mut app = App::new_for_test(tmp.path());
        use ratatui::crossterm::event::{KeyCode, KeyEvent};
        app.on_key(KeyEvent::from(KeyCode::Char('?')));
        let lines = screen(&mut app, 100, 30);
        println!("{}", lines.join("\n"));
        let all = lines.join("\n");
        assert!(all.contains(" Help "));
        assert!(all.contains("Panels"));
        assert!(all.contains("Ctrl+C") || all.contains("Space"), "{all}");
        app.on_key(KeyEvent::from(KeyCode::Esc));

        app.on_key(KeyEvent::from(KeyCode::Char('o')));
        app.on_key(KeyEvent::from(KeyCode::Right)); // язык → русский
        let lines = screen(&mut app, 100, 20);
        println!("{}", lines.join("\n"));
        let all = lines.join("\n");
        assert!(all.contains("Настройки"), "{all}");
        assert!(all.contains("‹ Русский ›"));
        assert!(all.contains("‹ Dark ›"));
        // Подсказки тоже на русском, без перезапуска.
        assert!(lines[19].contains("изменить"));
    }
}
