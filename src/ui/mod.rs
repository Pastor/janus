//! Состояние приложения, цикл событий и обработка клавиш.

mod finder;
mod help;
mod job;
mod large;
mod render;
pub mod theme;
mod viewer;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::{Receiver, Sender, never, select, tick, unbounded};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::archive::Format;
use crate::config::{Config, LEFT_PCT_STEP, Side, clamp_left_pct};
use crate::drives::{self, Drive};
use crate::editor::{Editor, OpenError};
use crate::git;
use crate::i18n::{Lang, Text};
use crate::ops::{self, Choice, Problem, Report, Transfer};
use crate::panel::{Panel, in_archive, mount_base};
use crate::preview::{Body, Loader, Preview};
use crate::search::Kind as SearchKind;
use crate::ui::finder::Finder;
use crate::ui::help::{Field, SettingsForm};
use crate::ui::job::{Job, JobKind, JobMsg};
use crate::ui::large::Large;
use crate::ui::theme::{Theme, ThemeName};
use crate::ui::viewer::{HSCROLL_STEP, Viewer};

/// Сколько ждать загрузчик просмотра, прежде чем счесть его зависшим.
const LOAD_TIMEOUT: Duration = Duration::from_secs(5);

/// Сообщение в строке состояния. Живёт до следующего нажатия клавиши.
pub struct Status {
    pub text: String,
    pub error: bool,
}

/// Что сейчас происходит на экране поверх панелей.
pub enum Mode {
    /// Обычная работа с панелями.
    Normal,
    /// Ожидание подтверждения удаления.
    ConfirmDelete {
        /// Безвозвратно, а не в корзину.
        permanent: bool,
        targets: Vec<PathBuf>,
    },
    /// Выполняется файловая операция.
    Job(Job),
    /// Подтверждение сохранения по Ctrl+S.
    EditorSave,
    /// Выход из редактора с несохранёнными изменениями.
    EditorExit,
    /// Создание архива: выбор формата и имени.
    Pack(PackForm),
    /// Справка; `scroll` — первая видимая строка.
    Help { scroll: usize },
    /// Окно настроек.
    Settings(SettingsForm),
    /// Выбор диска для активной панели.
    Drives(DrivesForm),
    /// Переименование элемента под курсором.
    Rename(RenameForm),
    /// Создание директории или файла в директории активной панели.
    Create(CreateForm),
}

/// Строка ввода имени с курсором.
pub struct NameInput {
    pub name: String,
    /// Позиция курсора в символах (не в байтах) от начала имени.
    pub cursor: usize,
}

impl NameInput {
    /// Строка с курсором в позиции `cursor`.
    fn new(name: String, cursor: usize) -> Self {
        Self { name, cursor }
    }

    /// Байтовое смещение курсора в строке.
    fn byte_at(&self, cursor: usize) -> usize {
        self.name
            .char_indices()
            .nth(cursor)
            .map_or(self.name.len(), |(i, _)| i)
    }

    /// Правка строки ввода. `false` — клавиша не про редактирование.
    fn edit(&mut self, code: KeyCode) -> bool {
        let len = self.name.chars().count();
        match code {
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte_at(self.cursor);
                self.name.remove(at);
            }
            KeyCode::Delete if self.cursor < len => {
                let at = self.byte_at(self.cursor);
                self.name.remove(at);
            }
            KeyCode::Char(c) => {
                let at = self.byte_at(self.cursor);
                self.name.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
}

/// Форма переименования: строка ввода с курсором.
pub struct RenameForm {
    /// Что переименовываем.
    pub source: PathBuf,
    /// Новое имя.
    pub input: NameInput,
}

impl RenameForm {
    /// Курсор ставится перед расширением: чаще всего меняют имя, а не тип.
    /// У директорий и имён вроде `.gitignore` — в конец.
    pub fn new(source: PathBuf, is_dir: bool) -> Self {
        let name = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let len = name.chars().count();
        let cursor = match name.rfind('.') {
            Some(dot) if !is_dir && dot > 0 => name[..dot].chars().count(),
            _ => len,
        };
        Self {
            source,
            input: NameInput::new(name, cursor),
        }
    }
}

/// Форма создания: пустая строка ввода для имени новой директории или файла.
pub struct CreateForm {
    /// Где создаём.
    pub dir: PathBuf,
    /// Директория (`m`) или файл (`n`).
    pub is_dir: bool,
    pub input: NameInput,
}

/// Окно выбора диска.
pub struct DrivesForm {
    pub drives: Vec<Drive>,
    /// Индекс выбранного диска.
    pub selected: usize,
}

/// Форма создания архива.
pub struct PackForm {
    /// Индекс в [`Format::PACKABLE`].
    pub format: usize,
    /// Имя архива без расширения.
    pub name: String,
    pub sources: Vec<PathBuf>,
}

impl PackForm {
    pub fn format(&self) -> Format {
        Format::PACKABLE[self.format]
    }

    /// Имя файла архива с расширением выбранного формата.
    pub fn file_name(&self) -> String {
        format!("{}.{}", self.name, self.format().extension())
    }
}

/// Состояние приложения.
pub struct App {
    /// Левая и правая панели, индексируются через [`App::panel`].
    panels: [Panel; 2],
    /// Активная панель.
    pub active: Side,
    /// Скрытая панель. Скрыть можно только неактивную: активная всегда видна.
    pub hidden: Option<Side>,
    /// Доля ширины окна под левую панель, в процентах.
    pub left_pct: u16,
    pub cfg: Config,
    pub theme: Theme,
    pub status: Option<Status>,
    pub mode: Mode,
    /// Сохранять конфиг при выходе: `false`, если файл был испорчен и
    /// пользователь ещё не сохранил настройки явно.
    pub save_config: bool,
    /// Тема и язык, которые пишутся в файл. Ключи `-t` / `-l` меняют их только
    /// на этот запуск; постоянными они становятся после сохранения настроек.
    stored_look: (ThemeName, Lang),
    /// Панель просмотра, если открыта. Она занимает место одной из панелей.
    pub viewer: Option<Viewer>,
    /// С какого момента просмотр ждёт загрузчик — чтобы не ждать вечно.
    loading_since: Option<Instant>,
    /// Редактор на месте просмотра, если открыт.
    pub editor: Option<Editor>,
    /// Поиск в панели, если открыт.
    pub finder: Option<Finder>,
    /// Режим «большие объекты», если открыт.
    pub large: Option<Large>,
    loader: Loader,
    /// Канал ответов git о незакоммиченных изменениях в панелях.
    git_tx: Sender<git::Reply<Side>>,
    git_rx: Receiver<git::Reply<Side>>,
    /// Для какого чтения директории каждой панели статус уже запрошен.
    git_asked: [u64; 2],
    pub quit: bool,
}

impl App {
    fn new(left: PathBuf, right: PathBuf, cfg: Config) -> Self {
        let theme = Theme::detect(cfg.theme);
        let session = cfg.session.clone();
        let mut status = None;
        let mut open = |dir: PathBuf, columns: u8| {
            Panel::open(dir.clone(), columns, cfg.show_hidden).unwrap_or_else(|err| {
                status = Some(Status {
                    text: format!("{}: {}: {err}", cfg.lang.text().cannot_open, dir.display()),
                    error: true,
                });
                Panel::empty(dir, columns)
            })
        };
        let panels = [
            open(left, session.left_columns),
            open(right, session.right_columns),
        ];
        let (git_tx, git_rx) = unbounded();
        Self {
            panels,
            active: Side::Left,
            hidden: session.hidden.filter(|&side| side != Side::Left),
            left_pct: clamp_left_pct(session.left_pct),
            theme,
            status,
            mode: Mode::Normal,
            save_config: true,
            stored_look: (cfg.theme, cfg.lang),
            cfg,
            viewer: None,
            editor: None,
            loading_since: None,
            finder: None,
            large: None,
            loader: Loader::new(),
            git_tx,
            git_rx,
            git_asked: [0; 2],
            quit: false,
        }
    }

    /// Приложение с обеими панелями в `dir` и настройками по умолчанию.
    #[cfg(test)]
    pub fn new_for_test(dir: &std::path::Path) -> Self {
        Self::new(dir.to_path_buf(), dir.to_path_buf(), Config::default())
    }

    /// Строки интерфейса на выбранном языке.
    pub fn text(&self) -> &'static Text {
        self.cfg.lang.text()
    }

    /// Панель на заданной стороне.
    pub fn panel(&self, side: Side) -> &Panel {
        &self.panels[side as usize]
    }

    pub fn panel_mut(&mut self, side: Side) -> &mut Panel {
        &mut self.panels[side as usize]
    }

    fn active_panel(&mut self) -> &mut Panel {
        self.panel_mut(self.active)
    }

    fn info(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            error: false,
        });
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
            error: true,
        });
    }

    /// Сообщает о неудачной попытке открыть директорию.
    fn fail_open(&mut self, dir: PathBuf, err: std::io::Error) {
        let text = format!("{}: {}: {err}", self.text().cannot_open, dir.display());
        self.fail(text);
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Операция сначала сворачивается: выход случится по её отчёту.
            if let Mode::Job(job) = &mut self.mode {
                job.cancel();
            }
            self.quit = true;
            return;
        }
        self.status = None;
        // Буква не из латиницы там, где ждут команду, — включена другая
        // раскладка. Молча ничего не делать хуже, чем сказать об этом.
        if !self.typing()
            && let KeyCode::Char(c) = key.code
            && c.is_alphabetic()
            && !c.is_ascii()
        {
            let text = self.text().wrong_layout;
            self.fail(text);
            return;
        }
        // Пока идёт операция или открыт диалог, клавиши принадлежат им.
        match self.mode {
            Mode::Normal => {}
            Mode::ConfirmDelete { .. } => return self.on_confirm_key(key.code),
            Mode::Job(_) => return self.on_job_key(key.code),
            Mode::EditorSave => return self.on_save_key(key.code),
            Mode::EditorExit => return self.on_exit_key(key.code),
            Mode::Pack(_) => return self.on_pack_key(key.code),
            Mode::Help { .. } => return self.on_help_key(key.code),
            Mode::Settings(_) => return self.on_settings_key(key.code),
            Mode::Drives(_) => return self.on_drives_key(key.code),
            Mode::Rename(_) => return self.on_rename_key(key.code),
            Mode::Create(_) => return self.on_create_key(key.code),
        }
        // Редактор сам разбирает сочетания с Ctrl.
        if self.editor.is_some() {
            self.status = None;
            return self.on_editor_key(key);
        }
        // Сочетания с Ctrl и Alt пока не используются; Shift входит в сам символ.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return;
        }
        self.status = None;

        if self.large.is_some() {
            self.on_large_key(key.code);
        } else if self.finder.is_some() {
            self.on_finder_key(key.code);
        } else if self.viewer_focused() {
            self.on_viewer_key(key.code);
        } else {
            self.on_files_key(key);
        }
        self.sync_viewer();
    }

    /// Сейчас вводится текст: буквы — это текст, а не команды.
    fn typing(&self) -> bool {
        match self.mode {
            Mode::Pack(_) | Mode::Rename(_) | Mode::Create(_) => true,
            Mode::Normal => {
                self.editor.is_some()
                    || self.finder.is_some()
                    || (self.viewer_focused()
                        && self.viewer.as_ref().is_some_and(|v| v.search.is_some()))
            }
            _ => false,
        }
    }

    /// Клавиши файловой панели.
    fn on_files_key(&mut self, key: KeyEvent) {
        let show_hidden = self.cfg.show_hidden;
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            // Shift+←→ при нескольких колонках — соседняя колонка, та же строка.
            KeyCode::Left | KeyCode::Right if shift => {
                let panel = self.active_panel();
                if panel.columns > 1 {
                    let step = panel.rows as isize;
                    panel.move_by(if key.code == KeyCode::Left {
                        -step
                    } else {
                        step
                    });
                }
            }
            KeyCode::Up => self.active_panel().move_by(-1),
            KeyCode::Down => self.active_panel().move_by(1),
            KeyCode::PageUp => {
                let page = self.active_panel().rows as isize;
                self.active_panel().move_by(-page);
            }
            KeyCode::PageDown => {
                let page = self.active_panel().rows as isize;
                self.active_panel().move_by(page);
            }
            KeyCode::Home => self.active_panel().home(),
            KeyCode::End => self.active_panel().end(),
            // Архив открывается как директория.
            KeyCode::Right | KeyCode::Enter
                if self
                    .active_panel()
                    .current()
                    .is_some_and(|e| !e.kind.is_dir() && Format::detect(&e.path).is_some()) =>
            {
                self.open_archive();
            }
            // Enter на файле открывает его в просмотре и переводит туда фокус.
            KeyCode::Enter
                if self
                    .active_panel()
                    .current()
                    .is_some_and(|e| !e.kind.is_dir()) =>
            {
                if self.viewer.is_none() {
                    self.open_viewer();
                }
                if let Some(viewer) = &self.viewer {
                    self.active = viewer.side;
                }
            }
            KeyCode::Right | KeyCode::Enter => {
                let target = self.active_panel().current().map(|e| e.path.clone());
                if let Err(err) = self.active_panel().enter(show_hidden)
                    && let Some(dir) = target
                {
                    self.fail_open(dir, err);
                }
            }
            KeyCode::Left => {
                let target = self.active_panel().dir.parent().map(PathBuf::from);
                match self.active_panel().leave(show_hidden) {
                    // Выше корня — другие диски: в Windows иначе на них не попасть.
                    Ok(false) => self.open_drives(),
                    Ok(true) => {}
                    Err(err) => {
                        if let Some(dir) = target {
                            self.fail_open(dir, err);
                        }
                    }
                }
            }
            KeyCode::Tab => {
                if self.hidden.is_none() {
                    self.active = self.active.other();
                }
            }
            KeyCode::Char('>') => self.widen(LEFT_PCT_STEP as i16),
            KeyCode::Char('<') => self.widen(-(LEFT_PCT_STEP as i16)),
            KeyCode::Char('p') => {
                if self.viewer.is_some() {
                    self.close_viewer();
                } else {
                    self.open_viewer();
                }
            }
            KeyCode::Esc => self.close_viewer(),
            KeyCode::Char('/') => self.open_finder(SearchKind::Content),
            KeyCode::Char('f') => self.open_finder(SearchKind::Name),
            KeyCode::Char('P') => {
                // Скрытие второй панели закрывает и просмотр на её месте.
                self.close_viewer();
                self.hidden = match self.hidden {
                    Some(_) => None,
                    None => Some(self.active.other()),
                };
            }
            KeyCode::Char(c @ '0'..='3') => {
                let n = c as u8 - b'0';
                self.active_panel().set_columns(n);
                let text = format!("{}: {n}", self.text().columns);
                self.info(text);
            }
            KeyCode::Char('.') => self.toggle_hidden(),
            KeyCode::Char(' ') => self.active_panel().toggle_mark(),
            KeyCode::Char('c') => self.start_transfer(Transfer::Copy),
            KeyCode::Char('v') => self.start_transfer(Transfer::Move),
            KeyCode::Char('d') => self.ask_delete(!self.cfg.use_trash),
            KeyCode::Char('D') => self.ask_delete(true),
            KeyCode::Char('a') => self.ask_pack(),
            KeyCode::Char('l') => {
                let root = self.panel(self.active).dir.clone();
                self.large = Some(Large::start(root));
            }
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.mode = Mode::Help { scroll: 0 },
            KeyCode::Char('o') => self.open_settings(),
            KeyCode::Char('g') => self.open_drives(),
            KeyCode::Char('r') | KeyCode::F(2) => self.ask_rename(),
            KeyCode::Char('m') => self.ask_create(true),
            KeyCode::Char('n') => self.ask_create(false),
            _ => {}
        }
    }

    /// Фокус в панели просмотра.
    fn viewer_focused(&self) -> bool {
        self.viewer.as_ref().is_some_and(|v| v.side == self.active)
    }

    /// Открывает просмотр на месте второй панели.
    fn open_viewer(&mut self) {
        self.hidden = None;
        self.viewer = Some(Viewer::new(self.active.other(), self.cfg.wrap));
        self.sync_viewer();
    }

    /// Закрывает просмотр; фокус возвращается в файловую панель.
    fn close_viewer(&mut self) {
        if let Some(viewer) = self.viewer.take() {
            self.active = viewer.side.other();
        }
    }

    /// Просмотр следует за курсором файловой панели: если под курсором
    /// другой путь, просит загрузчик прочитать его.
    fn sync_viewer(&mut self) {
        let Some(viewer) = &self.viewer else {
            return;
        };
        // Во время поиска просмотр показывает выбранную находку.
        if let Some(finder) = &self.finder {
            let Some(hit) = finder.current() else {
                return;
            };
            let wanted = (hit.path.clone(), hit.line);
            if finder.shown.as_ref() == Some(&wanted) {
                return;
            }
            let query = finder.query.clone();
            if viewer.path.as_ref() != Some(&wanted.0) {
                let request = self.loader.request(wanted.0.clone());
                self.loading_since = Some(Instant::now());
                if let Some(viewer) = &mut self.viewer {
                    viewer.show(wanted.0.clone(), request);
                }
            }
            if let (Some(viewer), Some(line)) = (&mut self.viewer, wanted.1) {
                viewer.focus(&query, line);
            }
            if let Some(finder) = &mut self.finder {
                finder.shown = Some(wanted);
            }
            return;
        }
        let panel = self.panel(viewer.side.other());
        let target = panel
            .current()
            .map(|e| e.path.clone())
            .unwrap_or_else(|| panel.dir.clone());
        if viewer.path.as_ref() == Some(&target) {
            return;
        }
        let request = self.loader.request(target.clone());
        self.loading_since = Some(Instant::now());
        if let Some(viewer) = &mut self.viewer {
            viewer.show(target, request);
        }
    }

    /// Открывает поиск в активной панели. Если вторая панель видна, в ней
    /// открывается просмотр находок.
    fn open_finder(&mut self, kind: SearchKind) {
        let root = self.panel(self.active).dir.clone();
        let mut finder = Finder::new(kind, self.active, root, self.cfg.show_hidden);
        if self.hidden.is_none() && self.viewer.is_none() {
            self.viewer = Some(Viewer::new(self.active.other(), self.cfg.wrap));
            finder.opened_viewer = true;
        }
        self.finder = Some(finder);
    }

    /// Закрывает поиск. `go` — перейти к выбранной находке в панели.
    fn close_finder(&mut self, go: bool) {
        let Some(finder) = self.finder.take() else {
            return;
        };
        let hit = finder.current().cloned().filter(|_| go);
        if let Some(hit) = &hit
            && let Some(dir) = hit.path.parent()
        {
            let show = self.cfg.show_hidden;
            let panel = self.panel_mut(finder.side);
            match panel.cd(dir.to_path_buf(), show) {
                Ok(()) => {
                    panel.select_name(&hit.path.file_name().unwrap_or_default().to_string_lossy())
                }
                Err(err) => self.fail_open(dir.to_path_buf(), err),
            }
        }
        // Просмотр, открытый поиском, уходит вместе с ним — если только
        // пользователь не перешёл к находке: тогда она остаётся на виду.
        if finder.opened_viewer && hit.is_none() {
            self.close_viewer();
        } else if let Some(viewer) = &mut self.viewer {
            viewer.search = None;
            if hit.is_none() {
                viewer.path = None;
            }
        }
        self.active = finder.side;
    }

    /// Клавиши поиска в панели.
    fn on_finder_key(&mut self, code: KeyCode) {
        let Some(finder) = &mut self.finder else {
            return;
        };
        match code {
            KeyCode::Char(c) => {
                let query = format!("{}{c}", finder.query);
                finder.set_query(query);
                finder.shown = None;
            }
            KeyCode::Backspace => {
                let mut query = finder.query.clone();
                query.pop();
                finder.set_query(query);
                finder.shown = None;
            }
            KeyCode::Up => finder.move_by(-1),
            KeyCode::Down => finder.move_by(1),
            KeyCode::PageUp => finder.move_by(-(finder.rows as isize)),
            KeyCode::PageDown => finder.move_by(finder.rows as isize),
            KeyCode::Home => finder.home(),
            KeyCode::End => finder.end(),
            KeyCode::Enter => self.close_finder(true),
            KeyCode::Esc => self.close_finder(false),
            _ => {}
        }
    }

    /// Сообщение фонового поиска.
    fn on_finder_msg(&mut self, msg: crate::search::Msg) {
        if let Some(finder) = &mut self.finder {
            finder.on_msg(msg);
        }
        // Первые находки сразу показываются в просмотре.
        self.sync_viewer();
    }

    /// Результат загрузчика.
    fn on_preview_loaded(&mut self, request: u64, preview: Preview) -> bool {
        let taken = self
            .viewer
            .as_mut()
            .is_some_and(|viewer| viewer.loaded(request, preview));
        if taken {
            self.loading_since = None;
        }
        taken
    }

    /// Просмотр ждёт загрузчик.
    fn preview_pending(&self) -> bool {
        self.viewer
            .as_ref()
            .is_some_and(|v| v.path.is_some() && v.content.is_none())
    }

    /// Загрузчик не ответил вовремя — значит, завис на этом файле. Поток не
    /// остановить, поэтому он бросается, а вместо него запускается новый;
    /// просмотр показывает ошибку, а не бесконечную «загрузку».
    fn check_loader(&mut self) -> bool {
        let stuck = self.preview_pending()
            && self
                .loading_since
                .is_some_and(|t| t.elapsed() >= LOAD_TIMEOUT);
        if !stuck {
            return false;
        }
        self.loader = Loader::new();
        self.loading_since = None;
        let message = self.text().load_timeout.to_string();
        if let Some(viewer) = &mut self.viewer
            && let Some(path) = viewer.path.clone()
        {
            viewer.content = Some(Preview {
                path,
                size: 0,
                modified: None,
                mode: String::new(),
                owner: String::new(),
                body: Body::Error(message),
            });
        }
        true
    }

    /// Клавиши панели просмотра.
    fn on_viewer_key(&mut self, code: KeyCode) {
        let Some(viewer) = &mut self.viewer else {
            return;
        };
        if viewer.search.is_some() {
            let query = viewer
                .search
                .as_ref()
                .map(|s| s.query.clone())
                .unwrap_or_default();
            match code {
                KeyCode::Char(c) => viewer.set_query(format!("{query}{c}")),
                KeyCode::Backspace => {
                    let mut query = query;
                    query.pop();
                    viewer.set_query(query);
                }
                KeyCode::Right | KeyCode::Enter => viewer.jump(1),
                KeyCode::Left => viewer.jump(-1),
                KeyCode::Up => viewer.scroll(-1),
                KeyCode::Down => viewer.scroll(1),
                KeyCode::PageUp => viewer.page(-1),
                KeyCode::PageDown => viewer.page(1),
                KeyCode::Esc => viewer.search = None,
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Up => viewer.scroll(-1),
            KeyCode::Down => viewer.scroll(1),
            KeyCode::PageUp => viewer.page(-1),
            KeyCode::PageDown => viewer.page(1),
            KeyCode::Home => viewer.home(),
            KeyCode::End => viewer.end(),
            KeyCode::Left => viewer.hscroll(-(HSCROLL_STEP as isize)),
            KeyCode::Right => viewer.hscroll(HSCROLL_STEP as isize),
            // У Markdown `w` переключает оформление, у остального — перенос.
            KeyCode::Char('w') => {
                if !viewer.toggle_rendered() {
                    viewer.toggle_wrap();
                }
            }
            KeyCode::Char('e') => self.start_editing(),
            KeyCode::Char('/') => {
                viewer.start_search();
            }
            KeyCode::Tab => self.active = self.active.other(),
            KeyCode::Esc | KeyCode::Char('p' | 'q') => self.close_viewer(),
            KeyCode::Char('?') => self.mode = Mode::Help { scroll: 0 },
            _ => {}
        }
    }

    /// Открывает показанный в просмотре файл на правку.
    fn start_editing(&mut self) {
        let Some(viewer) = &self.viewer else {
            return;
        };
        let Some(content) = &viewer.content else {
            return;
        };
        if !matches!(content.body, Body::Text(_)) {
            let text = self.text().not_text;
            self.fail(text);
            return;
        }
        let (path, top) = (content.path.clone(), viewer.source_top());
        if in_archive(&path) {
            let text = self.text().archive_readonly;
            self.fail(text);
            return;
        }
        match Editor::open(&path, self.cfg.indent as usize) {
            Ok(mut editor) => {
                // Курсор там, где пользователь читал.
                editor.down(top);
                editor.top = top;
                self.editor = Some(editor);
            }
            Err(err) => {
                let t = self.text();
                let text = match err {
                    OpenError::NotText => t.not_text.to_string(),
                    OpenError::TooLarge => t.too_large.to_string(),
                    OpenError::Io(err) => format!("{}: {err}", t.cannot_read),
                };
                self.fail(text);
            }
        }
    }

    /// Клавиши редактора.
    fn on_editor_key(&mut self, key: KeyEvent) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.modifiers.contains(KeyModifiers::ALT) {
            return;
        }
        let page = editor.rows.max(1);
        match key.code {
            KeyCode::Char('s') if ctrl => {
                if editor.modified {
                    self.mode = Mode::EditorSave;
                } else {
                    let text = self.text().nothing_to_save;
                    self.info(text);
                }
            }
            KeyCode::Home if ctrl => editor.file_start(),
            KeyCode::End if ctrl => editor.file_end(),
            _ if ctrl => {}
            KeyCode::Esc => {
                if editor.modified {
                    self.mode = Mode::EditorExit;
                } else {
                    self.close_editor();
                }
            }
            KeyCode::Char(c) => editor.insert(c),
            KeyCode::Enter => editor.newline(),
            KeyCode::Backspace => editor.backspace(),
            KeyCode::Delete => editor.delete(),
            KeyCode::Tab => editor.tab(),
            KeyCode::BackTab => editor.backtab(),
            KeyCode::Up => editor.up(1),
            KeyCode::Down => editor.down(1),
            KeyCode::Left => editor.left(),
            KeyCode::Right => editor.right(),
            KeyCode::Home => editor.home(),
            KeyCode::End => editor.end(),
            KeyCode::PageUp => editor.up(page),
            KeyCode::PageDown => editor.down(page),
            _ => {}
        }
    }

    /// Подтверждение сохранения: `y` или Enter сохраняют, остальное отменяет.
    fn on_save_key(&mut self, code: KeyCode) {
        self.mode = Mode::Normal;
        if matches!(code, KeyCode::Char('y') | KeyCode::Enter) {
            self.save_editor();
        }
    }

    /// Выход с несохранёнными изменениями: `y` — сохранить и закрыть,
    /// `n` — закрыть без сохранения, Esc — вернуться к правке.
    fn on_exit_key(&mut self, code: KeyCode) {
        self.mode = Mode::Normal;
        match code {
            KeyCode::Char('y') | KeyCode::Enter => {
                if self.save_editor() {
                    self.close_editor();
                }
            }
            KeyCode::Char('n') => self.close_editor(),
            _ => {}
        }
    }

    /// Записывает файл редактора; `false` — не удалось.
    fn save_editor(&mut self) -> bool {
        let Some(editor) = &mut self.editor else {
            return false;
        };
        match editor.save() {
            Ok(()) => {
                // Размер и время файла изменились — панели и просмотр тоже.
                self.refresh_panels();
                let text = self.text().saved;
                self.info(text);
                true
            }
            Err(err) => {
                let text = format!("{}: {err}", self.text().save_failed);
                self.fail(text);
                false
            }
        }
    }

    /// Закрывает редактор и перечитывает просмотр: файл мог измениться.
    fn close_editor(&mut self) {
        let top = self.editor.take().map(|e| e.top);
        if let Some(viewer) = &mut self.viewer {
            viewer.path = None;
        }
        self.sync_viewer();
        if let (Some(viewer), Some(top)) = (&mut self.viewer, top) {
            viewer.top = top;
        }
    }

    /// Запускает копирование или перенос во вторую панель.
    fn start_transfer(&mut self, op: Transfer) {
        if self.hidden.is_some() || self.viewer.is_some() {
            let text = self.text().no_second_panel;
            self.fail(text);
            return;
        }
        let sources = self.panel(self.active).targets();
        if sources.is_empty() {
            return;
        }
        let dest = self.panel(self.active.other()).dir.clone();
        if in_archive(&dest) {
            let text = self.text().archive_readonly;
            self.fail(text);
            return;
        }
        if let Err(problem) = ops::check(&sources, &dest) {
            let text = self.text();
            self.fail(match problem {
                Problem::SameDir => text.same_dir,
                Problem::IntoItself => text.into_itself,
            });
            return;
        }
        self.mode = Mode::Job(Job::transfer(op, sources, dest, self.active));
    }

    /// Открывает подтверждение удаления.
    fn ask_delete(&mut self, permanent: bool) {
        let targets = self.panel(self.active).targets();
        if !targets.is_empty() {
            self.mode = Mode::ConfirmDelete { permanent, targets };
        }
    }

    /// Клавиши диалога удаления: повтор той же клавиши подтверждает,
    /// любая другая отменяет.
    fn on_confirm_key(&mut self, code: KeyCode) {
        let Mode::ConfirmDelete { permanent, targets } =
            std::mem::replace(&mut self.mode, Mode::Normal)
        else {
            return;
        };
        let confirm = if permanent { 'D' } else { 'd' };
        if code == KeyCode::Char(confirm) {
            self.mode = Mode::Job(Job::delete(targets, !permanent, self.active));
        }
    }

    /// Клавиши во время операции: ответы на конфликт и отмена.
    fn on_job_key(&mut self, code: KeyCode) {
        let Mode::Job(job) = &mut self.mode else {
            return;
        };
        if job.conflict.is_some() {
            let choice = match code {
                KeyCode::Char('r') => Choice::Replace,
                KeyCode::Char('s') => Choice::Skip,
                KeyCode::Char('R') => Choice::ReplaceAll,
                KeyCode::Char('S') => Choice::SkipAll,
                KeyCode::Esc | KeyCode::Char('q') => Choice::Cancel,
                _ => return,
            };
            job.answer(choice);
        } else if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
            job.cancel();
        }
    }

    /// Сообщение от фоновой операции.
    fn on_job_msg(&mut self, msg: JobMsg) {
        let Mode::Job(job) = &mut self.mode else {
            return;
        };
        match msg {
            JobMsg::Progress(progress) => job.progress = progress,
            JobMsg::Conflict(path, reply) => {
                job.conflict = Some((path, reply));
                // Отмену могли запросить, пока вопрос был в пути.
                if job.cancelling {
                    job.answer(Choice::Cancel);
                }
            }
            JobMsg::Done(report) => {
                let (kind, source, mount) = (job.kind, job.source, job.mount.take());
                self.mode = Mode::Normal;
                if let Some((archive, root)) = mount {
                    self.finish_extract(source, archive, root, report);
                } else {
                    self.finish_job(kind, source, report);
                }
            }
        }
    }

    /// Поток операции исчез, не прислав отчёта (например, паника).
    fn on_job_lost(&mut self) {
        if let Mode::Job(job) = std::mem::replace(&mut self.mode, Mode::Normal) {
            self.panel_mut(job.source).marked.clear();
            if let Some((_, root)) = job.mount {
                let _ = std::fs::remove_dir_all(root);
            }
        }
        self.refresh_panels();
        let text = self.text().job_lost;
        self.fail(text);
    }

    /// Распаковывает архив под курсором во временную директорию; панель
    /// войдёт в неё, когда распаковка закончится.
    fn open_archive(&mut self) {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let Some(archive) = self.active_panel().current().map(|e| e.path.clone()) else {
            return;
        };
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = archive.file_name().unwrap_or_default().to_string_lossy();
        let root = mount_base().join(format!("{n}-{name}"));
        if let Err(err) = std::fs::create_dir_all(&root) {
            self.fail_open(root, err);
            return;
        }
        self.mode = Mode::Job(Job::extract(archive, root, self.active));
    }

    /// Итог распаковки: при успехе панель входит в архив, иначе временная
    /// директория удаляется.
    fn finish_extract(&mut self, side: Side, archive: PathBuf, root: PathBuf, report: Report) {
        if report.done == 0 {
            let _ = std::fs::remove_dir_all(&root);
            let text = summary(self.text(), JobKind::Extract, &report);
            if report.errors.is_empty() {
                self.info(text);
            } else {
                self.fail(text);
            }
            return;
        }
        let show = self.cfg.show_hidden;
        if let Err(err) = self.panel_mut(side).mount(archive, root.clone(), show) {
            self.fail_open(root, err);
        }
    }

    /// Клавиши режима «большие объекты». Esc возвращает в панели и открывает
    /// в активной ту директорию, где был курсор.
    fn on_large_key(&mut self, code: KeyCode) {
        let Some(large) = &mut self.large else {
            return;
        };
        match code {
            KeyCode::Up => large.move_by(-1),
            KeyCode::Down => large.move_by(1),
            KeyCode::PageUp => large.move_by(-(large.rows as isize)),
            KeyCode::PageDown => large.move_by(large.rows as isize),
            KeyCode::Home => large.home(),
            KeyCode::End => large.end(),
            KeyCode::Right | KeyCode::Enter => {
                large.enter();
            }
            KeyCode::Left => {
                large.leave();
            }
            // Удаляется объект под курсором; подтверждение — как в панелях.
            KeyCode::Char(c @ ('d' | 'D')) => {
                if let Some(path) = large.selected_path() {
                    self.mode = Mode::ConfirmDelete {
                        permanent: c == 'D' || !self.cfg.use_trash,
                        targets: vec![path],
                    };
                }
            }
            KeyCode::Char('?') => self.mode = Mode::Help { scroll: 0 },
            KeyCode::Esc | KeyCode::Char('q') => {
                let Some(large) = self.large.take() else {
                    return;
                };
                // Сканирование не закончилось — переходить некуда.
                if large.tree.is_none() {
                    return;
                }
                let dir = large.current_path();
                let name = large.selected().map(|n| n.display_name().into_owned());
                if dir != self.panel(self.active).dir {
                    let show = self.cfg.show_hidden;
                    if let Err(err) = self.active_panel().cd(dir.clone(), show) {
                        self.fail_open(dir, err);
                        return;
                    }
                }
                if let Some(name) = name {
                    self.active_panel().select_name(&name);
                }
            }
            _ => {}
        }
    }

    /// Клавиши справки: прокрутка и закрытие.
    fn on_help_key(&mut self, code: KeyCode) {
        let Mode::Help { scroll } = &mut self.mode else {
            return;
        };
        let max = help::line_count(self.cfg.lang.text()).saturating_sub(1);
        match code {
            KeyCode::Up => *scroll = scroll.saturating_sub(1),
            KeyCode::Down => *scroll = (*scroll + 1).min(max),
            KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
            KeyCode::PageDown => *scroll = (*scroll + 10).min(max),
            KeyCode::Home => *scroll = 0,
            KeyCode::End => *scroll = max,
            KeyCode::Esc | KeyCode::Char('?' | 'q') => self.mode = Mode::Normal,
            _ => {}
        }
    }

    /// Открывает окно настроек.
    fn open_settings(&mut self) {
        self.mode = Mode::Settings(SettingsForm {
            field: 0,
            original: self.cfg.clone(),
        });
    }

    /// Клавиши настроек: ↑↓ — поле, ←→ и пробел — значение (применяется
    /// сразу), Enter — сохранить, Esc — вернуть как было.
    fn on_settings_key(&mut self, code: KeyCode) {
        let Mode::Settings(form) = &mut self.mode else {
            return;
        };
        let n = Field::ALL.len();
        let field = Field::ALL[form.field];
        let step = match code {
            KeyCode::Up => {
                form.field = (form.field + n - 1) % n;
                return;
            }
            KeyCode::Down | KeyCode::Tab => {
                form.field = (form.field + 1) % n;
                return;
            }
            KeyCode::Left => -1,
            KeyCode::Right | KeyCode::Char(' ') => 1,
            KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.store_session();
                // Явно сохранённые тема и язык — уже не разовое переопределение.
                self.stored_look = (self.cfg.theme, self.cfg.lang);
                match self.cfg.save() {
                    Ok(path) => {
                        // Явное сохранение — согласие перезаписать и испорченный файл.
                        self.save_config = true;
                        let text = format!("{}: {}", self.text().settings_saved, path.display());
                        self.info(text);
                    }
                    Err(err) => {
                        let text = format!("{}: {err:#}", self.text().config_not_saved);
                        self.fail(text);
                    }
                }
                return;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                let Mode::Settings(form) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return;
                };
                let before = std::mem::replace(&mut self.cfg, form.original);
                self.apply_settings(&before);
                return;
            }
            _ => return,
        };
        let before = self.cfg.clone();
        field.change(&mut self.cfg, step);
        self.apply_settings(&before);
    }

    /// Применяет изменившиеся настройки к тому, что уже на экране.
    fn apply_settings(&mut self, before: &Config) {
        if before.theme != self.cfg.theme {
            self.theme = Theme::detect(self.cfg.theme);
        }
        if before.wrap != self.cfg.wrap
            && let Some(viewer) = &mut self.viewer
        {
            viewer.wrap = self.cfg.wrap;
        }
        if before.show_hidden != self.cfg.show_hidden {
            self.refresh_panels();
        }
    }

    /// Открывает окно выбора диска; курсор — на диске активной панели.
    fn open_drives(&mut self) {
        let drives = drives::list();
        if drives.is_empty() {
            let text = self.text().no_drives;
            self.fail(text);
            return;
        }
        // Изнутри архива панель «стоит» там, где лежит сам архив.
        let panel = self.panel(self.active);
        let dir = panel.display_dir();
        let selected = drives::containing(&drives, &dir).unwrap_or(0);
        self.mode = Mode::Drives(DrivesForm { drives, selected });
    }

    /// Клавиши окна дисков: ↑↓ — выбор, Enter и → — перейти, Esc — закрыть.
    /// В Windows буква диска сразу переходит на него (кроме `q` — это Esc).
    fn on_drives_key(&mut self, code: KeyCode) {
        let Mode::Drives(form) = &mut self.mode else {
            return;
        };
        let last = form.drives.len().saturating_sub(1);
        match code {
            KeyCode::Up => form.selected = form.selected.saturating_sub(1),
            KeyCode::Down => form.selected = (form.selected + 1).min(last),
            KeyCode::Home | KeyCode::PageUp => form.selected = 0,
            KeyCode::End | KeyCode::PageDown => form.selected = last,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Left => self.mode = Mode::Normal,
            KeyCode::Enter | KeyCode::Right => self.go_drive(None),
            KeyCode::Char(c) if cfg!(windows) && c.is_ascii_alphabetic() => {
                let name = format!("{}:", c.to_ascii_uppercase());
                if let Some(i) = form.drives.iter().position(|d| d.name == name) {
                    self.go_drive(Some(i));
                }
            }
            _ => {}
        }
    }

    /// Закрывает окно дисков и открывает в активной панели выбранный диск
    /// (или диск `index`).
    fn go_drive(&mut self, index: Option<usize>) {
        let Mode::Drives(form) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        let Some(drive) = form.drives.into_iter().nth(index.unwrap_or(form.selected)) else {
            return;
        };
        if drive.path == self.panel(self.active).dir {
            return;
        }
        let show = self.cfg.show_hidden;
        if let Err(err) = self.active_panel().cd(drive.path.clone(), show) {
            self.fail_open(drive.path, err);
        }
    }

    /// Открывает форму переименования для элемента под курсором.
    fn ask_rename(&mut self) {
        let panel = self.panel(self.active);
        let Some(entry) = panel.current() else {
            return;
        };
        if in_archive(&panel.dir) {
            let text = self.text().archive_readonly;
            self.fail(text);
            return;
        }
        let form = RenameForm::new(entry.path.clone(), entry.kind.is_dir());
        self.mode = Mode::Rename(form);
    }

    /// Клавиши формы переименования: правка имени, Enter — применить, Esc — отмена.
    fn on_rename_key(&mut self, code: KeyCode) {
        let Mode::Rename(form) = &mut self.mode else {
            return;
        };
        match code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let Mode::Rename(form) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return;
                };
                self.rename(form.source, form.input.name);
            }
            code => {
                form.input.edit(code);
            }
        }
    }

    /// Переименовывает `source` в `name` в той же директории и ставит курсор
    /// на новое имя. Ошибка оставляет всё как было и сообщается в строке состояния.
    fn rename(&mut self, source: PathBuf, name: String) {
        let t = self.text();
        let name = name.trim().to_string();
        let old = source.file_name().map(|n| n.to_string_lossy().into_owned());
        if old.as_deref() == Some(name.as_str()) {
            return;
        }
        if !valid_name(&name) {
            self.fail(format!("{}: {name}", t.rename_invalid));
            return;
        }
        let Some(dir) = source.parent() else {
            return;
        };
        let target = dir.join(&name);
        // Смена только регистра (`a.txt` → `A.txt`) на нечувствительной к нему
        // файловой системе — это тот же файл, а не конфликт.
        let case_only = old.is_some_and(|o| o.to_lowercase() == name.to_lowercase());
        if !case_only && std::fs::symlink_metadata(&target).is_ok() {
            self.fail(format!("{}: {name}", t.rename_exists));
            return;
        }
        if let Err(err) = std::fs::rename(&source, &target) {
            self.fail(format!("{}: {err}", t.rename_failed));
            return;
        }
        self.refresh_panels();
        let side = self.active;
        self.panel_mut(side).select_name(&name);
        self.info(format!("{}: {name}", t.renamed));
    }

    /// Открывает форму создания директории (`is_dir`) или файла в директории
    /// активной панели.
    fn ask_create(&mut self, is_dir: bool) {
        let dir = self.panel(self.active).dir.clone();
        if in_archive(&dir) {
            let text = self.text().archive_readonly;
            self.fail(text);
            return;
        }
        self.mode = Mode::Create(CreateForm {
            dir,
            is_dir,
            input: NameInput::new(String::new(), 0),
        });
    }

    /// Клавиши формы создания: правка имени, Enter — создать, Esc — отмена.
    fn on_create_key(&mut self, code: KeyCode) {
        let Mode::Create(form) = &mut self.mode else {
            return;
        };
        match code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter => {
                let Mode::Create(form) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return;
                };
                self.create(form);
            }
            code => {
                form.input.edit(code);
            }
        }
    }

    /// Создаёт пустую директорию или файл и ставит на него курсор. Существующее
    /// не затирается, ошибка сообщается в строке состояния.
    fn create(&mut self, form: CreateForm) {
        let t = self.text();
        let name = form.input.name.trim().to_string();
        if !valid_name(&name) {
            self.fail(format!("{}: {name}", t.rename_invalid));
            return;
        }
        let target = form.dir.join(&name);
        if std::fs::symlink_metadata(&target).is_ok() {
            self.fail(format!("{}: {name}", t.rename_exists));
            return;
        }
        let made = if form.is_dir {
            std::fs::create_dir(&target)
        } else {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map(drop)
        };
        if let Err(err) = made {
            self.fail(format!("{}: {err}", t.create_failed));
            return;
        }
        self.refresh_panels();
        let side = self.active;
        self.panel_mut(side).select_name(&name);
        self.info(format!("{}: {name}", t.created));
    }

    /// Открывает форму создания архива для выделенного или элемента под курсором.
    fn ask_pack(&mut self) {
        let panel = self.panel(self.active);
        let sources = panel.targets();
        if sources.is_empty() {
            return;
        }
        if in_archive(&panel.dir) {
            let text = self.text().archive_readonly;
            self.fail(text);
            return;
        }
        // Один элемент — архив по его имени, несколько — по имени директории.
        let base = match sources.as_slice() {
            [one] => one.file_name(),
            _ => panel.dir.file_name(),
        };
        let name = base
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "archive".to_string());
        self.mode = Mode::Pack(PackForm {
            format: 0,
            name,
            sources,
        });
    }

    /// Клавиши формы архива: ←→ и Tab — формат, буквы — имя, Enter — создать.
    fn on_pack_key(&mut self, code: KeyCode) {
        let Mode::Pack(form) = &mut self.mode else {
            return;
        };
        let n = Format::PACKABLE.len();
        match code {
            KeyCode::Left | KeyCode::BackTab => form.format = (form.format + n - 1) % n,
            KeyCode::Right | KeyCode::Tab => form.format = (form.format + 1) % n,
            KeyCode::Char(c) if c != '/' => form.name.push(c),
            KeyCode::Backspace => {
                form.name.pop();
            }
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Enter if !form.name.trim().is_empty() => {
                let Mode::Pack(form) = std::mem::replace(&mut self.mode, Mode::Normal) else {
                    return;
                };
                let dest = self.panel(self.active).dir.join(form.file_name());
                self.mode = Mode::Job(Job::pack(form.format(), form.sources, dest, self.active));
            }
            _ => {}
        }
    }

    /// Обновляет панели после операции и сообщает итог.
    fn finish_job(&mut self, kind: JobKind, source: Side, report: Report) {
        self.panel_mut(source).marked.clear();
        self.refresh_panels();
        // Удаление из режима «большие объекты»: исчезнувший объект убирается
        // из дерева без повторного сканирования.
        if let Some(large) = &mut self.large
            && large
                .selected_path()
                .is_some_and(|path| std::fs::symlink_metadata(path).is_err())
        {
            large.remove_selected();
        }
        let text = summary(self.text(), kind, &report);
        if report.errors.is_empty() {
            self.info(text);
        } else {
            self.fail(text);
        }
    }

    /// Запрашивает статус git для панелей, директорию которых перечитали
    /// после прошлого запроса. Внутри распакованного архива git не нужен.
    fn request_git(&mut self) {
        for side in [Side::Left, Side::Right] {
            let panel = &self.panels[side as usize];
            if self.git_asked[side as usize] == panel.generation {
                continue;
            }
            self.git_asked[side as usize] = panel.generation;
            if in_archive(&panel.dir) {
                continue;
            }
            let dirs = panel
                .entries
                .iter()
                .filter(|e| e.kind.is_dir())
                .map(|e| e.name.clone())
                .collect();
            git::spawn(
                side,
                panel.generation,
                panel.dir.clone(),
                dirs,
                self.git_tx.clone(),
            );
        }
    }

    /// Применяет ответ git, если панель с тех пор не перечитывали.
    fn on_git_reply(&mut self, reply: git::Reply<Side>) -> bool {
        let panel = self.panel_mut(reply.key);
        if panel.generation != reply.generation || panel.vcs_dirty == reply.dirty {
            return false;
        }
        panel.vcs_dirty = reply.dirty;
        true
    }

    /// Перечитывает обе панели. Если директория панели исчезла (её удалили
    /// или перенесли), панель поднимается до ближайшего существующего предка.
    fn refresh_panels(&mut self) {
        let show = self.cfg.show_hidden;
        for side in [Side::Left, Side::Right] {
            let panel = self.panel_mut(side);
            if panel.reload(show).is_ok() {
                continue;
            }
            let mut dir = panel.dir.clone();
            while !dir.is_dir() {
                match dir.parent() {
                    Some(parent) => dir = parent.to_path_buf(),
                    None => break,
                }
            }
            if let Err(err) = panel.cd(dir.clone(), show) {
                self.fail_open(dir, err);
            }
        }
        // Показанный файл мог измениться или исчезнуть — перечитываем.
        if let Some(viewer) = &mut self.viewer {
            viewer.path = None;
        }
        self.sync_viewer();
    }

    /// Расширяет активную панель на `delta` процентов (сужает при отрицательном).
    fn widen(&mut self, delta: i16) {
        if self.hidden.is_some() {
            return;
        }
        let delta = match self.active {
            Side::Left => delta,
            Side::Right => -delta,
        };
        self.left_pct = clamp_left_pct((self.left_pct as i16 + delta).max(0) as u16);
    }

    /// Переключает показ скрытых файлов в обеих панелях.
    fn toggle_hidden(&mut self) {
        self.cfg.show_hidden = !self.cfg.show_hidden;
        let show = self.cfg.show_hidden;
        for side in [Side::Left, Side::Right] {
            if let Err(err) = self.panel_mut(side).reload(show) {
                let dir = self.panel(side).dir.clone();
                self.fail_open(dir, err);
                return;
            }
        }
        let text = self.text();
        self.info(if show {
            text.hidden_shown
        } else {
            text.hidden_concealed
        });
    }

    /// Переносит состояние окна в конфиг перед сохранением.
    /// Конфиг для записи при выходе: с темой и языком из файла, а не из
    /// ключей командной строки.
    fn config_to_save(&self) -> Config {
        let mut cfg = self.cfg.clone();
        (cfg.theme, cfg.lang) = self.stored_look;
        cfg
    }

    fn store_session(&mut self) {
        let session = &mut self.cfg.session;
        session.right = Some(self.panels[Side::Right as usize].dir.clone());
        session.left_pct = self.left_pct;
        session.left_columns = self.panels[Side::Left as usize].columns;
        session.right_columns = self.panels[Side::Right as usize].columns;
        session.hidden = self.hidden;
    }
}

impl Drop for App {
    /// Панели удаляют свои распакованные архивы сами; общая папка процесса
    /// удаляется, только если опустела — её может делить другой экземпляр
    /// приложения в том же процессе (так бывает в тестах).
    fn drop(&mut self) {
        for panel in &mut self.panels {
            panel.mounts.iter().for_each(|m| {
                let _ = std::fs::remove_dir_all(&m.root);
            });
            panel.mounts.clear();
        }
        let _ = std::fs::remove_dir(mount_base());
    }
}

/// Поток чтения событий терминала: главный цикл ждёт сразу несколько
/// источников (позже — фоновые операции), поэтому события идут через канал.
fn spawn_events() -> Receiver<Event> {
    let (tx, rx): (Sender<Event>, Receiver<Event>) = unbounded();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(ev).is_err() {
                break;
            }
        }
    });
    rx
}

/// Точка входа TUI: инициализация терминала, главный цикл, сохранение сессии.
///
/// `cfg_ok` — конфиг прочитан без ошибок. Если нет, файл не перезаписывается:
/// иначе одна опечатка стоила бы пользователю всех его настроек.
/// Возвращает директорию активной панели на момент выхода.
/// `stored_look` — тема и язык из файла настроек, до переопределения ключами
/// командной строки: при выходе в файл пишутся они.
pub fn run(
    left: PathBuf,
    right: PathBuf,
    cfg: Config,
    cfg_ok: bool,
    stored_look: (ThemeName, Lang),
) -> Result<PathBuf> {
    let mut app = App::new(left, right, cfg);
    app.save_config = cfg_ok;
    app.stored_look = stored_look;
    let mut terminal = ratatui::init();
    // Паники загрузчика перехватываются в нём самом; обработчик ratatui при этом
    // восстановил бы терминал и сломал экран работающей программы.
    let restore_terminal = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some(crate::preview::LOADER_THREAD) {
            restore_terminal(info);
        }
    }));
    let result = main_loop(&mut terminal, &mut app);
    ratatui::restore();
    // Распакованные архивы живут только пока работает программа.
    let _ = std::fs::remove_dir_all(mount_base());
    result?;
    let last = app.panel(app.active).exit_dir();

    if app.save_config {
        app.store_session();
        if let Err(err) = app.config_to_save().save() {
            eprintln!("{}: {err:#}", app.text().config_not_saved);
        }
    }
    Ok(last)
}

/// Итог операции одной строкой: «скопировано: 3, пропущено: 1, ошибок: 2 — …».
fn summary(text: &Text, kind: JobKind, report: &Report) -> String {
    let verb = match kind {
        JobKind::Copy => text.done_copy,
        JobKind::Move => text.done_move,
        JobKind::Trash => text.done_trash,
        JobKind::Delete => text.done_delete,
        JobKind::Extract => text.done_extract,
        JobKind::Pack => text.done_pack,
    };
    let mut parts = vec![format!("{verb}: {}", report.done)];
    if report.skipped > 0 {
        parts.push(format!("{}: {}", text.skipped, report.skipped));
    }
    if report.cancelled {
        parts.push(text.cancelled.to_string());
    }
    if let Some(first) = report.errors.first() {
        parts.push(format!(
            "{}: {} — {first}",
            text.errors,
            report.errors.len()
        ));
    }
    parts.join(", ")
}

fn main_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let events = spawn_events();
    // Канал загрузчика живёт столько же, сколько приложение.
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| render::draw(frame, app))?;
            dirty = false;
        }
        // Выход ждёт окончания операции: оборванное копирование хуже задержки.
        if app.quit && !matches!(app.mode, Mode::Job(_)) {
            return Ok(());
        }
        let jobs = match &app.mode {
            Mode::Job(job) => job.rx.clone(),
            _ => never(),
        };
        let found = app
            .finder
            .as_ref()
            .and_then(Finder::rx)
            .unwrap_or_else(never);
        let scanned = app.large.as_ref().and_then(Large::rx).unwrap_or_else(never);
        // Пока идёт сканирование, счётчики на экране обновляются по таймеру.
        // Загрузчик могли заменить — канал берётся каждый раз заново.
        let previews = app.loader.rx.clone();
        dirty |= app.check_loader();
        app.request_git();
        let git_replies = app.git_rx.clone();
        let ticks = if app.large.as_ref().is_some_and(|l| l.tree.is_none()) || app.preview_pending()
        {
            tick(std::time::Duration::from_millis(100))
        } else {
            never()
        };
        select! {
            recv(scanned) -> tree => {
                if let (Ok(tree), Some(large)) = (tree, app.large.as_mut()) {
                    large.scanned(tree);
                }
                dirty = true;
            }
            recv(ticks) -> _ => dirty = true,
            recv(found) -> msg => {
                if let Ok(msg) = msg {
                    app.on_finder_msg(msg);
                    dirty = true;
                }
            }
            recv(jobs) -> msg => {
                match msg {
                    Ok(msg) => app.on_job_msg(msg),
                    Err(_) => app.on_job_lost(),
                }
                dirty = true;
            }
            recv(git_replies) -> reply => {
                if let Ok(reply) = reply {
                    dirty |= app.on_git_reply(reply);
                }
            }
            recv(previews) -> loaded => {
                if let Ok((request, preview)) = loaded {
                    dirty |= app.on_preview_loaded(request, preview);
                }
            }
            recv(events) -> ev => match ev {
                Ok(Event::Key(key)) => { app.on_key(key); dirty = true; }
                Ok(Event::Resize(_, _)) => dirty = true,
                Ok(_) => {}
                // Поток чтения событий умер — работать дальше нечем.
                Err(_) => return Ok(()),
            },
        }
    }
}

/// Имя годится для элемента в директории. Разделитель пути превратил бы
/// переименование в перенос, а создание — в создание в другом месте.
fn valid_name(name: &str) -> bool {
    !(name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || (cfg!(windows) && name.contains(['\\', ':', '*', '?', '"', '<', '>', '|'])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LEFT_PCT_MAX, LEFT_PCT_MIN};

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn app_in(dir: &std::path::Path) -> App {
        App::new_for_test(dir)
    }

    #[test]
    fn tab_switches_panels_unless_one_is_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, Side::Right);

        // Скрывается неактивная панель, переключение блокируется.
        press(&mut app, KeyCode::Char('P'));
        assert_eq!(app.hidden, Some(Side::Left));
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, Side::Right);

        press(&mut app, KeyCode::Char('P'));
        assert_eq!(app.hidden, None);
    }

    #[test]
    fn width_keys_grow_the_active_panel() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Char('>'));
        assert_eq!(app.left_pct, 50 + LEFT_PCT_STEP);

        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('>'));
        press(&mut app, KeyCode::Char('>'));
        assert_eq!(app.left_pct, 50 - LEFT_PCT_STEP);

        for _ in 0..50 {
            press(&mut app, KeyCode::Char('>'));
        }
        assert_eq!(app.left_pct, LEFT_PCT_MIN);
        for _ in 0..50 {
            press(&mut app, KeyCode::Char('<'));
        }
        assert_eq!(app.left_pct, LEFT_PCT_MAX);
    }

    #[test]
    fn digits_set_columns_of_active_panel() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Char('3'));
        assert_eq!(app.panel(Side::Left).columns, 3);
        assert_eq!(app.panel(Side::Right).columns, 1);
        press(&mut app, KeyCode::Char('0'));
        assert_eq!(app.panel(Side::Left).columns, 0);
        assert_eq!(app.panel(Side::Left).layout_columns(), 1);
        assert!(!app.panel(Side::Left).details());
    }

    #[test]
    fn dot_toggles_hidden_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".secret"), "").unwrap();
        std::fs::write(tmp.path().join("plain"), "").unwrap();
        let mut app = app_in(tmp.path());
        assert_eq!(app.panel(Side::Left).entries.len(), 2);
        press(&mut app, KeyCode::Char('.'));
        assert_eq!(app.panel(Side::Left).entries.len(), 1);
        assert_eq!(app.panel(Side::Right).entries.len(), 1);
        assert!(!app.cfg.show_hidden);
    }

    #[test]
    fn arrows_enter_and_leave_directories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Right);
        assert_eq!(app.panel(Side::Left).dir, tmp.path().join("sub"));
        press(&mut app, KeyCode::Left);
        assert_eq!(app.panel(Side::Left).dir, tmp.path());
        assert_eq!(app.panel(Side::Left).current().unwrap().name, "sub");
    }

    #[test]
    fn session_is_stored_into_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Char('P'));
        app.store_session();
        assert_eq!(app.cfg.session.left_columns, 2);
        assert_eq!(app.cfg.session.hidden, Some(Side::Right));
        assert_eq!(app.cfg.session.right.as_deref(), Some(tmp.path()));
    }

    /// Дожидается конца операции, отвечая на конфликты клавишей `answer`.
    pub(super) fn finish(app: &mut App, answer: KeyCode) {
        while let Mode::Job(job) = &app.mode {
            let msg = job.rx.recv().expect("операция должна прислать отчёт");
            app.on_job_msg(msg);
            if matches!(&app.mode, Mode::Job(job) if job.conflict.is_some()) {
                press(app, answer);
            }
        }
    }

    /// Две директории: `left` с файлами и пустая `right`; правая панель в `right`.
    fn two_dirs() -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        let (left, right) = (tmp.path().join("left"), tmp.path().join("right"));
        std::fs::create_dir_all(left.join("dir")).unwrap();
        std::fs::write(left.join("dir/inner"), "i").unwrap();
        std::fs::write(left.join("a"), "a").unwrap();
        std::fs::write(left.join("b"), "b").unwrap();
        std::fs::create_dir_all(&right).unwrap();
        let app = App::new(left, right, Config::default());
        (tmp, app)
    }

    #[test]
    fn copy_marked_files_to_other_panel() {
        let (tmp, mut app) = two_dirs();
        // Порядок: dir/, a, b. Выделяем dir и b.
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char(' '));
        assert_eq!(app.panel(Side::Left).marked.len(), 2);
        press(&mut app, KeyCode::Char('c'));
        finish(&mut app, KeyCode::Esc);

        let right = tmp.path().join("right");
        assert!(right.join("dir/inner").exists());
        assert!(right.join("b").exists());
        assert!(!right.join("a").exists());
        // Панели перечитаны, выделение снято, итог в строке состояния.
        assert_eq!(app.panel(Side::Right).entries.len(), 2);
        assert!(app.panel(Side::Left).marked.is_empty());
        let status = app.status.as_ref().unwrap();
        assert!(!status.error);
        assert!(status.text.contains("copied: 2"), "{}", status.text);
    }

    #[test]
    fn move_without_marks_takes_item_under_cursor() {
        let (tmp, mut app) = two_dirs();
        press(&mut app, KeyCode::Down); // a
        press(&mut app, KeyCode::Char('v'));
        finish(&mut app, KeyCode::Esc);
        assert!(!tmp.path().join("left/a").exists());
        assert!(tmp.path().join("right/a").exists());
        assert_eq!(app.panel(Side::Left).entries.len(), 2);
    }

    #[test]
    fn conflict_is_answered_from_keyboard() {
        let (tmp, mut app) = two_dirs();
        std::fs::write(tmp.path().join("right/a"), "old").unwrap();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Char('c'));
        finish(&mut app, KeyCode::Char('s'));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("right/a")).unwrap(),
            "old"
        );
        assert!(app.status.as_ref().unwrap().text.contains("skipped: 1"));

        press(&mut app, KeyCode::Char('c'));
        finish(&mut app, KeyCode::Char('r'));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("right/a")).unwrap(),
            "a"
        );
    }

    #[test]
    fn transfer_needs_a_visible_distinct_panel() {
        let (_tmp, mut app) = two_dirs();
        press(&mut app, KeyCode::Char('P'));
        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.as_ref().unwrap().error);

        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), "").unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Char('v'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.as_ref().unwrap().text.contains("same directory"));
    }

    #[test]
    fn delete_requires_repeated_key() {
        let (tmp, mut app) = two_dirs();
        press(&mut app, KeyCode::Down); // a
        // Любая другая клавиша отменяет удаление.
        press(&mut app, KeyCode::Char('D'));
        assert!(matches!(
            app.mode,
            Mode::ConfirmDelete {
                permanent: true,
                ..
            }
        ));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(tmp.path().join("left/a").exists());

        // Для безвозвратного удаления нужно именно `D`, а не `d`.
        press(&mut app, KeyCode::Char('D'));
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(tmp.path().join("left/a").exists());

        press(&mut app, KeyCode::Char('D'));
        press(&mut app, KeyCode::Char('D'));
        finish(&mut app, KeyCode::Esc);
        assert!(!tmp.path().join("left/a").exists());
        assert!(app.status.as_ref().unwrap().text.contains("deleted: 1"));
    }

    #[test]
    fn delete_in_large_mode_updates_tree() {
        let (tmp, mut app) = two_dirs();
        press(&mut app, KeyCode::Char('l'));
        let rx = app.large.as_ref().unwrap().rx().unwrap();
        let tree = rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        app.large.as_mut().unwrap().scanned(tree);
        let target = app.large.as_ref().unwrap().selected_path().unwrap();
        let count = app.large.as_ref().unwrap().order.len();

        press(&mut app, KeyCode::Char('D'));
        assert!(matches!(
            app.mode,
            Mode::ConfirmDelete {
                permanent: true,
                ..
            }
        ));
        press(&mut app, KeyCode::Char('D'));
        finish(&mut app, KeyCode::Esc);
        assert!(!target.exists());
        let large = app.large.as_ref().expect("режим остаётся открытым");
        assert_eq!(large.order.len(), count - 1);
        assert!(app.status.as_ref().unwrap().text.contains("deleted: 1"));
        assert!(tmp.path().exists());
    }

    #[test]
    fn panel_inside_deleted_directory_climbs_up() {
        let (tmp, mut app) = two_dirs();
        let dir = tmp.path().join("left/dir");
        app.panel_mut(Side::Right).cd(dir.clone(), true).unwrap();
        // Удаляем dir из левой панели безвозвратно.
        press(&mut app, KeyCode::Char('D'));
        press(&mut app, KeyCode::Char('D'));
        finish(&mut app, KeyCode::Esc);
        assert!(!dir.exists());
        assert_eq!(app.panel(Side::Right).dir, tmp.path().join("left"));
    }

    /// Дожидается загрузки просмотра.
    pub(super) fn wait_preview(app: &mut App) {
        let rx = app.loader.rx.clone();
        while app.viewer.as_ref().is_some_and(|v| v.content.is_none()) {
            let (request, preview) = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("загрузчик должен ответить");
            app.on_preview_loaded(request, preview);
        }
    }

    fn viewer_dir() -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let code: String = (1..=60).map(|i| format!("let x{i} = {i};\n")).collect();
        std::fs::write(tmp.path().join("main.rs"), code).unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "hello").unwrap();
        let app = app_in(tmp.path());
        (tmp, app)
    }

    #[test]
    fn p_opens_viewer_that_follows_cursor() {
        let (tmp, mut app) = viewer_dir();
        press(&mut app, KeyCode::Char('p'));
        let viewer = app.viewer.as_ref().unwrap();
        assert_eq!(viewer.side, Side::Right);
        assert_eq!(
            viewer.path.as_deref(),
            Some(tmp.path().join("sub").as_path())
        );
        // Фокус остаётся в списке файлов.
        assert_eq!(app.active, Side::Left);

        press(&mut app, KeyCode::Down);
        assert_eq!(
            app.viewer.as_ref().unwrap().path.as_deref(),
            Some(tmp.path().join("main.rs").as_path())
        );
        wait_preview(&mut app);
        assert!(app.viewer.as_ref().unwrap().content.is_some());

        // Копировать некуда: вторая панель занята просмотром.
        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.as_ref().unwrap().error);

        press(&mut app, KeyCode::Char('p'));
        assert!(app.viewer.is_none());
    }

    #[test]
    fn enter_on_file_focuses_viewer_and_esc_returns() {
        let (_tmp, mut app) = viewer_dir();
        press(&mut app, KeyCode::Down); // main.rs
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.active, Side::Right);
        wait_preview(&mut app);

        // Клавиши идут в просмотр: прокрутка, перенос.
        app.viewer.as_mut().unwrap().fit(10, 50);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.viewer.as_ref().unwrap().top, 10);
        press(&mut app, KeyCode::Char('w'));
        assert!(!app.viewer.as_ref().unwrap().wrap);

        // Tab — обратно в список, курсор там не двигался.
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.active, Side::Left);
        assert_eq!(app.panel(Side::Left).current().unwrap().name, "main.rs");
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Esc);
        assert!(app.viewer.is_none());
        assert_eq!(app.active, Side::Left);
    }

    #[test]
    fn search_typed_in_viewer() {
        let (_tmp, mut app) = viewer_dir();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Enter);
        wait_preview(&mut app);
        press(&mut app, KeyCode::Char('/'));
        for c in "x5".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        let search = app.viewer.as_ref().unwrap().search.as_ref().unwrap();
        // x5 и x50..x59.
        assert_eq!(search.matches.len(), 11);
        press(&mut app, KeyCode::Right);
        assert_eq!(
            app.viewer
                .as_ref()
                .unwrap()
                .search
                .as_ref()
                .unwrap()
                .current,
            1
        );
        press(&mut app, KeyCode::Backspace);
        assert_eq!(
            app.viewer.as_ref().unwrap().search.as_ref().unwrap().query,
            "x"
        );
        // Esc закрывает только поиск, просмотр остаётся.
        press(&mut app, KeyCode::Esc);
        assert!(app.viewer.as_ref().unwrap().search.is_none());
        assert!(app.viewer.is_some());
    }

    #[test]
    fn hiding_second_panel_closes_viewer() {
        let (_tmp, mut app) = viewer_dir();
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Char('P'));
        assert!(app.viewer.is_none());
        assert_eq!(app.hidden, Some(Side::Right));
        // Просмотр снова показывает вторую панель.
        press(&mut app, KeyCode::Char('p'));
        assert_eq!(app.hidden, None);
    }

    fn ctrl(app: &mut App, c: char) {
        app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
    }

    /// Открывает `main.rs` в редакторе.
    fn editing() -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("main.rs"), "fn main() {\n}\n").unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Enter);
        wait_preview(&mut app);
        press(&mut app, KeyCode::Char('e'));
        assert!(app.editor.is_some());
        (tmp, app)
    }

    #[test]
    fn edit_and_save_with_confirmation() {
        let (tmp, mut app) = editing();
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Enter);
        for c in "run();".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        // Клавиши ушли в редактор, а не в панели: `e`, `p`, `q` — просто буквы.
        assert_eq!(app.editor.as_ref().unwrap().lines[1], "    run();");

        ctrl(&mut app, 's');
        assert!(matches!(app.mode, Mode::EditorSave));
        // Отказ — файл не тронут.
        press(&mut app, KeyCode::Esc);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("main.rs")).unwrap(),
            "fn main() {\n}\n"
        );

        ctrl(&mut app, 's');
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("main.rs")).unwrap(),
            "fn main() {\n    run();\n}\n"
        );
        assert!(!app.editor.as_ref().unwrap().modified);
        assert!(app.status.as_ref().unwrap().text.contains("saved"));

        // Без изменений Esc закрывает сразу; просмотр перечитывает файл.
        press(&mut app, KeyCode::Esc);
        assert!(app.editor.is_none());
        assert!(app.viewer.is_some());
        wait_preview(&mut app);
    }

    #[test]
    fn esc_with_changes_asks_and_can_discard() {
        let (tmp, mut app) = editing();
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::EditorExit));
        // Esc в диалоге — назад к правке.
        press(&mut app, KeyCode::Esc);
        assert!(app.editor.is_some());
        assert!(matches!(app.mode, Mode::Normal));

        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('n'));
        assert!(app.editor.is_none());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("main.rs")).unwrap(),
            "fn main() {\n}\n"
        );
    }

    #[test]
    fn exit_dialog_can_save() {
        let (tmp, mut app) = editing();
        press(&mut app, KeyCode::Char('x'));
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('y'));
        assert!(app.editor.is_none());
        assert!(
            std::fs::read_to_string(tmp.path().join("main.rs"))
                .unwrap()
                .starts_with("xfn")
        );
    }

    #[test]
    fn directories_cannot_be_edited() {
        let (_tmp, mut app) = viewer_dir();
        press(&mut app, KeyCode::Char('p')); // курсор на sub/
        press(&mut app, KeyCode::Tab);
        wait_preview(&mut app);
        press(&mut app, KeyCode::Char('e'));
        assert!(app.editor.is_none());
        assert!(app.status.as_ref().unwrap().error);
    }

    /// Дожидается окончания поиска в панели.
    pub(super) fn wait_finder(app: &mut App) {
        while let Some(rx) = app.finder.as_ref().and_then(Finder::rx) {
            let msg = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("поиск должен закончиться");
            app.on_finder_msg(msg);
        }
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn search_tree() -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("deep/er")).unwrap();
        std::fs::write(tmp.path().join("top.txt"), "nothing\nTODO top\n").unwrap();
        std::fs::write(tmp.path().join("deep/er/inner.rs"), "// TODO inner\n").unwrap();
        let app = app_in(tmp.path());
        (tmp, app)
    }

    #[test]
    fn content_search_opens_viewer_and_esc_restores() {
        let (_tmp, mut app) = search_tree();
        press(&mut app, KeyCode::Char('/'));
        // Просмотр открыт поиском во второй панели.
        assert!(app.viewer.is_some());
        type_text(&mut app, "todo");
        wait_finder(&mut app);
        let finder = app.finder.as_ref().unwrap();
        assert_eq!(finder.hits.len(), 2);
        assert_eq!(finder.query, "todo");
        // Буквы ушли в запрос, а не в команды панели (`d`, `o`).
        assert!(matches!(app.mode, Mode::Normal));

        wait_preview(&mut app);
        let viewer = app.viewer.as_ref().unwrap();
        assert!(viewer.path.as_ref().unwrap().ends_with("top.txt"));
        assert_eq!(viewer.search.as_ref().unwrap().current().unwrap().line, 1);

        press(&mut app, KeyCode::Down);
        assert!(
            app.viewer
                .as_ref()
                .unwrap()
                .path
                .as_ref()
                .unwrap()
                .ends_with("inner.rs")
        );

        press(&mut app, KeyCode::Esc);
        assert!(app.finder.is_none());
        assert!(
            app.viewer.is_none(),
            "просмотр, открытый поиском, закрывается"
        );
    }

    #[test]
    fn enter_goes_to_found_file() {
        let (tmp, mut app) = search_tree();
        press(&mut app, KeyCode::Char('/'));
        type_text(&mut app, "inner");
        wait_finder(&mut app);
        press(&mut app, KeyCode::Enter);
        assert!(app.finder.is_none());
        let panel = app.panel(Side::Left);
        assert_eq!(panel.dir, tmp.path().join("deep/er"));
        assert_eq!(panel.current().unwrap().name, "inner.rs");
        // Просмотр остаётся на найденном файле, без режима поиска.
        let viewer = app.viewer.as_ref().unwrap();
        assert!(viewer.search.is_none());
        assert!(viewer.path.as_ref().unwrap().ends_with("inner.rs"));
    }

    #[test]
    fn name_search_finds_directories() {
        let (tmp, mut app) = search_tree();
        press(&mut app, KeyCode::Char('f'));
        type_text(&mut app, "er");
        wait_finder(&mut app);
        let names: Vec<String> = app
            .finder
            .as_ref()
            .unwrap()
            .hits
            .iter()
            .map(|h| h.text.clone())
            .collect();
        assert_eq!(names, ["er", "inner.rs"]);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.panel(Side::Left).dir, tmp.path().join("deep"));
        assert_eq!(app.panel(Side::Left).current().unwrap().name, "er");
    }

    #[test]
    fn search_without_second_panel_keeps_it_hidden() {
        let (_tmp, mut app) = search_tree();
        press(&mut app, KeyCode::Char('P'));
        press(&mut app, KeyCode::Char('/'));
        assert!(app.viewer.is_none());
        assert_eq!(app.hidden, Some(Side::Right));
        type_text(&mut app, "x");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.finder.as_ref().unwrap().query, "");
        press(&mut app, KeyCode::Esc);
        assert!(app.finder.is_none());
    }

    /// Директория с архивом `a.zip`, где лежат `docs/readme.txt` и `x.txt`.
    fn with_zip() -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join("docs")).unwrap();
        std::fs::write(src.join("docs/readme.txt"), "read").unwrap();
        let left = tmp.path().join("left");
        std::fs::create_dir_all(&left).unwrap();
        let mut control = crate::archive::tests_support::Silent;
        let report = crate::archive::pack(
            Format::Zip,
            &[src.join("docs")],
            &left.join("a.zip"),
            &mut control,
        );
        assert_eq!(report.done, 1);
        std::fs::create_dir_all(tmp.path().join("right")).unwrap();
        let app = App::new(left, tmp.path().join("right"), Config::default());
        (tmp, app)
    }

    #[test]
    fn archive_opens_like_directory_and_cleans_up() {
        let (tmp, mut app) = with_zip();
        press(&mut app, KeyCode::Enter);
        finish(&mut app, KeyCode::Esc);
        let panel = app.panel(Side::Left);
        assert_eq!(panel.display_dir(), tmp.path().join("left/a.zip"));
        assert_eq!(panel.current().unwrap().name, "docs");
        let root = panel.mounts[0].root.clone();
        assert!(root.starts_with(mount_base()));

        // Внутрь архива копировать нельзя, из архива — можно.
        std::fs::write(tmp.path().join("right/f.txt"), "").unwrap();
        app.refresh_panels();
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('c'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.status.as_ref().unwrap().text.contains("read-only"));
        assert!(!root.join("f.txt").exists());
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('c'));
        finish(&mut app, KeyCode::Esc);
        assert!(tmp.path().join("right/docs/readme.txt").exists());

        // Выход — курсор на архиве, временная папка удалена.
        press(&mut app, KeyCode::Left);
        assert_eq!(app.panel(Side::Left).dir, tmp.path().join("left"));
        assert_eq!(app.panel(Side::Left).current().unwrap().name, "a.zip");
        assert!(!root.exists());
    }

    #[test]
    fn broken_archive_reports_error_and_stays() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bad.zip"), "garbage").unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Right);
        finish(&mut app, KeyCode::Esc);
        assert_eq!(app.panel(Side::Left).dir, tmp.path());
        assert!(app.panel(Side::Left).mounts.is_empty());
        assert!(app.status.as_ref().unwrap().error);
    }

    #[test]
    fn pack_marked_items_with_chosen_name() {
        let (tmp, mut app) = two_dirs();
        press(&mut app, KeyCode::Down); // a
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' ')); // a, b
        press(&mut app, KeyCode::Char('a'));
        let Mode::Pack(form) = &app.mode else {
            panic!("ожидалась форма архива");
        };
        // Несколько элементов — имя по директории.
        assert_eq!(form.file_name(), "left.zip");
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Backspace);
        press(&mut app, KeyCode::Backspace);
        type_text(&mut app, "ab");
        press(&mut app, KeyCode::Left); // по кругу на tar.zst
        press(&mut app, KeyCode::Enter);
        finish(&mut app, KeyCode::Esc);
        let archive = tmp.path().join("left/ab.tar.zst");
        let names: Vec<String> = crate::archive::list(&archive)
            .unwrap()
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names, ["a", "b"]);
        assert!(app.status.as_ref().unwrap().text.contains("packed: 2"));
    }

    #[test]
    fn pack_dialog_esc_cancels() {
        let (tmp, mut app) = two_dirs();
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!tmp.path().join("left/dir.zip").exists());
    }

    /// Дожидается окончания сканирования в режиме «большие объекты».
    pub(super) fn wait_large(app: &mut App) {
        let rx = app
            .large
            .as_ref()
            .and_then(Large::rx)
            .expect("режим открыт");
        let tree = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("сканирование должно закончиться");
        app.large.as_mut().unwrap().scanned(tree);
    }

    #[test]
    fn large_mode_esc_opens_directory_in_panel() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("big/deeper")).unwrap();
        std::fs::write(tmp.path().join("big/deeper/blob"), vec![0u8; 5000]).unwrap();
        std::fs::write(tmp.path().join("big/note"), "x").unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Char('l'));
        wait_large(&mut app);
        press(&mut app, KeyCode::Enter); // big
        press(&mut app, KeyCode::Down); // note
        press(&mut app, KeyCode::Esc);
        assert!(app.large.is_none());
        let panel = app.panel(Side::Left);
        assert_eq!(panel.dir, tmp.path().join("big"));
        assert_eq!(panel.current().unwrap().name, "note");
    }

    #[test]
    fn large_mode_esc_during_scan_just_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Char('l'));
        // Клавиши режима не трогают панель, даже `d` и `c`.
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(app.mode, Mode::Normal));
        app.large.as_mut().unwrap().tree = None;
        press(&mut app, KeyCode::Esc);
        assert!(app.large.is_none());
        assert_eq!(app.panel(Side::Left).dir, tmp.path());
    }

    #[test]
    fn settings_apply_live_and_esc_reverts() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".dot"), "").unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Char('o'));
        press(&mut app, KeyCode::Down); // тема
        press(&mut app, KeyCode::Right);
        assert_eq!(app.theme.name, crate::ui::theme::ThemeName::Light);
        press(&mut app, KeyCode::Down); // скрытые
        press(&mut app, KeyCode::Char(' '));
        assert!(app.panel(Side::Left).entries.is_empty());

        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert_eq!(app.cfg.theme, crate::ui::theme::ThemeName::Dark);
        assert_eq!(app.theme.name, crate::ui::theme::ThemeName::Dark);
        assert_eq!(app.panel(Side::Left).entries.len(), 1);
    }

    #[test]
    fn help_scrolls_and_closes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Char('?'));
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::End);
        let Mode::Help { scroll } = app.mode else {
            panic!("ожидалась справка");
        };
        assert_eq!(scroll, help::line_count(app.text()) - 1);
        press(&mut app, KeyCode::Char('?'));
        assert!(matches!(app.mode, Mode::Normal));
        // Справка доступна и из просмотра.
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('?'));
        assert!(matches!(app.mode, Mode::Help { .. }));
    }

    #[test]
    fn shift_arrows_move_between_columns() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..25 {
            std::fs::write(tmp.path().join(format!("f{i:02}")), "").unwrap();
        }
        let mut app = app_in(tmp.path());
        let shift = |app: &mut App, code| app.on_key(KeyEvent::new(code, KeyModifiers::SHIFT));

        // Одна колонка: Shift+стрелки ничего не делают — ни курсор, ни директория.
        shift(&mut app, KeyCode::Right);
        assert_eq!(app.panel(Side::Left).cursor, 0);
        assert_eq!(app.panel(Side::Left).dir, tmp.path());

        press(&mut app, KeyCode::Char('3'));
        app.panel_mut(Side::Left).rows = 10; // высоту задаёт отрисовка
        press(&mut app, KeyCode::Down);
        shift(&mut app, KeyCode::Right);
        assert_eq!(app.panel(Side::Left).cursor, 11);
        shift(&mut app, KeyCode::Right);
        assert_eq!(app.panel(Side::Left).cursor, 21);
        // Дальше колонки нет — курсор упирается в последний элемент.
        shift(&mut app, KeyCode::Right);
        assert_eq!(app.panel(Side::Left).cursor, 24);
        shift(&mut app, KeyCode::Left);
        assert_eq!(app.panel(Side::Left).cursor, 14);
        shift(&mut app, KeyCode::Left);
        shift(&mut app, KeyCode::Left);
        assert_eq!(app.panel(Side::Left).cursor, 0);
        assert_eq!(app.panel(Side::Left).dir, tmp.path());
    }

    #[test]
    fn q_acts_like_esc_outside_the_file_list() {
        let (_tmp, mut app) = viewer_dir();
        // Просмотр: q закрывает его, а не программу.
        press(&mut app, KeyCode::Char('p'));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.viewer.is_none());
        assert!(!app.quit);

        // Диалог удаления: q — отмена.
        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('q'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!app.quit);

        // Режим «большие объекты»: q — назад в панели.
        press(&mut app, KeyCode::Char('l'));
        wait_large(&mut app);
        press(&mut app, KeyCode::Char('q'));
        assert!(app.large.is_none());
        assert!(!app.quit);

        // В строке поиска q — просто буква.
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('q'));
        assert_eq!(app.finder.as_ref().unwrap().query, "q");
        press(&mut app, KeyCode::Esc);

        // В списке файлов q — выход.
        press(&mut app, KeyCode::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn foreign_layout_is_reported_not_ignored() {
        let (tmp, mut app) = two_dirs();
        press(&mut app, KeyCode::Down);
        // «с» — это `c` в русской раскладке.
        press(&mut app, KeyCode::Char('с'));
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!tmp.path().join("right/a").exists());
        let status = app.status.as_ref().unwrap();
        assert!(status.error);
        assert!(status.text.contains("keyboard layout"), "{}", status.text);

        // Там, где вводится текст, кириллица — обычный ввод.
        press(&mut app, KeyCode::Char('/'));
        press(&mut app, KeyCode::Char('с'));
        assert_eq!(app.finder.as_ref().unwrap().query, "с");
    }

    #[test]
    fn stuck_loader_is_replaced_and_reported() {
        let (_tmp, mut app) = viewer_dir();
        press(&mut app, KeyCode::Char('p'));
        // Изображаем зависший загрузчик: ответа нет, а срок вышел.
        app.loader = Loader::new();
        app.loading_since = Some(Instant::now() - LOAD_TIMEOUT);
        assert!(app.preview_pending());
        assert!(app.check_loader());
        let viewer = app.viewer.as_ref().unwrap();
        assert!(matches!(
            viewer.content.as_ref().map(|c| &c.body),
            Some(Body::Error(_))
        ));
        assert!(!app.check_loader());

        // Новый загрузчик работает: следующий файл грузится как обычно.
        press(&mut app, KeyCode::Down);
        wait_preview(&mut app);
        assert!(app.viewer.as_ref().unwrap().content.is_some());
    }

    #[test]
    fn java_annotations_preview_loads() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("App.java"),
            "@SpringBootApplication\npublic class App {\n    @Override\n    public String toString() { return \"x\"; }\n}\n",
        )
        .unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Enter);
        wait_preview(&mut app);
        let viewer = app.viewer.as_ref().unwrap();
        let Some(Body::Text(text)) = viewer.content.as_ref().map(|c| &c.body) else {
            panic!("ожидался текст");
        };
        assert_eq!(text.plain(0), "@SpringBootApplication");
    }

    #[test]
    fn drives_dialog_opens_and_switches_panel() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(tmp.path());
        press(&mut app, KeyCode::Char('g'));
        let Mode::Drives(form) = &app.mode else {
            panic!("ожидалось окно дисков");
        };
        // Курсор стоит на диске, где открыта панель.
        let here = &form.drives[form.selected];
        assert!(tmp.path().starts_with(&here.path));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));

        // Первый диск — корень (в Unix `/`): переходим на него.
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        let root = app.panel(app.active).dir.clone();
        assert!(root.parent().is_none(), "{}", root.display());

        // ← в корне открывает то же окно.
        press(&mut app, KeyCode::Left);
        assert!(matches!(app.mode, Mode::Drives(_)));
    }

    #[test]
    fn command_line_look_is_not_saved_unless_settings_are_saved() {
        let tmp = tempfile::tempdir().unwrap();
        let mut app = app_in(tmp.path());
        // Как после `-t nord -l ru` при светлой английской теме в файле.
        app.stored_look = (ThemeName::Light, Lang::En);
        app.cfg.theme = ThemeName::Nord;
        app.cfg.lang = Lang::Ru;
        let saved = app.config_to_save();
        assert_eq!((saved.theme, saved.lang), (ThemeName::Light, Lang::En));
        assert_eq!(saved.session.right_columns, app.cfg.session.right_columns);

        // Сохранение в окне настроек делает выбор постоянным.
        app.stored_look = (app.cfg.theme, app.cfg.lang);
        let saved = app.config_to_save();
        assert_eq!((saved.theme, saved.lang), (ThemeName::Nord, Lang::Ru));
    }

    fn rename_dir() -> (tempfile::TempDir, App) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("report.txt"), "x").unwrap();
        std::fs::write(tmp.path().join("taken.txt"), "y").unwrap();
        let app = app_in(tmp.path());
        (tmp, app)
    }

    #[test]
    fn rename_cursor_stops_before_extension() {
        let form = RenameForm::new(PathBuf::from("/x/report.tar.gz"), false);
        assert_eq!(form.input.cursor, "report.tar".len());
        let form = RenameForm::new(PathBuf::from("/x/.gitignore"), false);
        assert_eq!(form.input.cursor, ".gitignore".len());
        let form = RenameForm::new(PathBuf::from("/x/v1.2"), true);
        assert_eq!(form.input.cursor, "v1.2".len());
    }

    #[test]
    fn rename_edits_name_and_keeps_cursor_on_it() {
        let (tmp, mut app) = rename_dir();
        app.active_panel().select_name("report.txt");
        press(&mut app, KeyCode::Char('r'));
        assert!(matches!(app.mode, Mode::Rename(_)));
        // Курсор перед `.txt`: стираем «report», пишем кириллицей —
        // в поле ввода это текст, а не команда в чужой раскладке.
        for _ in 0..6 {
            press(&mut app, KeyCode::Backspace);
        }
        type_text(&mut app, "отчёт");
        press(&mut app, KeyCode::End);
        press(&mut app, KeyCode::Char('2'));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(tmp.path().join("отчёт.txt2").is_file());
        assert!(!tmp.path().join("report.txt").exists());
        assert_eq!(app.panel(app.active).current().unwrap().name, "отчёт.txt2");
    }

    #[test]
    fn rename_refuses_existing_and_invalid_names() {
        let (tmp, mut app) = rename_dir();
        app.active_panel().select_name("report.txt");
        for name in ["taken.txt", "a/b", ""] {
            press(&mut app, KeyCode::F(2));
            let Mode::Rename(form) = &mut app.mode else {
                panic!("ожидалась форма переименования");
            };
            form.input.name = name.to_string();
            press(&mut app, KeyCode::Enter);
            assert!(app.status.as_ref().is_some_and(|s| s.error), "{name}");
        }
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("taken.txt")).unwrap(),
            "y"
        );
        assert!(tmp.path().join("report.txt").is_file());

        // Esc ничего не меняет.
        press(&mut app, KeyCode::Char('r'));
        type_text(&mut app, "zzz");
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(tmp.path().join("report.txt").is_file());
    }

    #[test]
    fn create_makes_directory_and_file_under_cursor() {
        let (tmp, mut app) = rename_dir();
        press(&mut app, KeyCode::Char('m'));
        assert!(matches!(
            app.mode,
            Mode::Create(CreateForm { is_dir: true, .. })
        ));
        // Кириллица в поле ввода — текст, а не команда в чужой раскладке.
        type_text(&mut app, "новая");
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(tmp.path().join("новая").is_dir());
        assert_eq!(app.panel(app.active).current().unwrap().name, "новая");

        press(&mut app, KeyCode::Char('n'));
        type_text(&mut app, "notes.md");
        press(&mut app, KeyCode::Enter);
        let file = tmp.path().join("notes.md");
        assert!(file.is_file());
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 0);
        assert_eq!(app.panel(app.active).current().unwrap().name, "notes.md");
    }

    #[test]
    fn create_refuses_existing_and_invalid_names() {
        let (tmp, mut app) = rename_dir();
        for (key, name) in [
            ('n', "taken.txt"),
            ('m', "taken.txt"),
            ('m', "a/b"),
            ('n', " "),
        ] {
            press(&mut app, KeyCode::Char(key));
            type_text(&mut app, name);
            press(&mut app, KeyCode::Enter);
            assert!(app.status.as_ref().is_some_and(|s| s.error), "{name}");
        }
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("taken.txt")).unwrap(),
            "y"
        );
        assert!(!tmp.path().join("a").exists());

        // Esc ничего не создаёт.
        press(&mut app, KeyCode::Char('m'));
        type_text(&mut app, "zzz");
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal));
        assert!(!tmp.path().join("zzz").exists());
    }
}
