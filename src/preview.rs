//! Содержимое панели просмотра и его фоновая загрузка.
//!
//! Чтение и подсветка большого файла, а тем более подсчёт размера директории,
//! могут занять заметное время. Поэтому всё это делает отдельный поток, а
//! интерфейс получает готовый результат. Курсор бегает по списку быстрее, чем
//! грузятся файлы, так что поток всегда берёт только самый свежий запрос, а
//! подсчёт размера директории бросает, как только пришёл новый.

use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender, unbounded};
use unicode_width::UnicodeWidthStr;

use crate::highlight::{self, Highlighter, Piece};

/// Больше этого текстового файла не читаем: просмотр — не редактор.
const TEXT_LIMIT: u64 = 8 * 1024 * 1024;
/// Предел числа строк по той же причине.
const LINE_LIMIT: usize = 100_000;
/// Сколько байт показывать в hex-дампе двоичного файла.
const HEX_LIMIT: usize = 4 * 1024;
/// По этому началу файла решается, текст это или нет.
const SNIFF: usize = 8 * 1024;
/// Табуляция раскрывается в столько пробелов.
const TAB_WIDTH: usize = 4;
/// Подсчёт размера директории не идёт дольше этого.
const DIR_TIME_LIMIT: Duration = Duration::from_secs(3);
/// Сколько крупнейших элементов директории показывать.
const LARGEST: usize = 10;

/// Текстовый файл, разобранный на подсвеченные строки.
#[derive(Debug)]
pub struct TextBody {
    /// Фрагменты каждой строки; склеенные, они дают строку целиком.
    pub lines: Vec<Vec<Piece>>,
    /// Ширина каждой строки в колонках терминала — для переноса.
    pub widths: Vec<usize>,
    /// Название языка, если он распознан.
    pub language: Option<&'static str>,
    /// Показывать номера строк.
    pub numbered: bool,
    /// Файл прочитан не целиком.
    pub truncated: bool,
    /// Оформленный вид для Markdown.
    pub markdown: Option<crate::markdown::Doc>,
}

impl TextBody {
    /// Текст строки без разметки.
    pub fn plain(&self, line: usize) -> String {
        self.lines[line].iter().map(|p| p.text.as_str()).collect()
    }
}

/// Сведения о директории.
#[derive(Debug, Default)]
pub struct DirInfo {
    pub files: u64,
    pub dirs: u64,
    pub links: u64,
    pub hidden: u64,
    /// Суммарный размер всего поддерева.
    pub total_size: u64,
    /// Число элементов во всём поддереве.
    pub total_items: u64,
    /// Подсчёт завершён; иначе числа — нижняя граница.
    pub complete: bool,
    /// Крупнейшие элементы верхнего уровня: имя, размер, директория ли.
    pub largest: Vec<(String, u64, bool)>,
}

/// Что показывать.
#[derive(Debug)]
pub enum Body {
    Text(TextBody),
    Dir(DirInfo),
    /// Строки hex-дампа начала файла.
    Binary(Vec<String>),
    /// Содержимое архива.
    Archive(Vec<crate::archive::Item>),
    /// Не удалось прочитать.
    Error(String),
}

/// Готовый к показу просмотр.
#[derive(Debug)]
pub struct Preview {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Права в виде `drwxr-xr-x`.
    pub mode: String,
    /// Владелец и группа: `501:20`.
    pub owner: String,
    pub body: Body,
}

/// Имя потока загрузчика: по нему обработчик паник узнаёт, что паника
/// перехвачена здесь и терминал восстанавливать не нужно.
pub const LOADER_THREAD: &str = "preview-loader";

/// Фоновый загрузчик просмотров.
pub struct Loader {
    tx: Sender<(u64, PathBuf)>,
    pub rx: Receiver<(u64, Preview)>,
    next: u64,
}

impl Loader {
    pub fn new() -> Self {
        let (tx, requests) = unbounded::<(u64, PathBuf)>();
        let (results, rx) = unbounded();
        let spawned = std::thread::Builder::new()
            .name(LOADER_THREAD.into())
            .spawn(move || {
                while let Ok(mut request) = requests.recv() {
                    // Пока грузилось предыдущее, могли прийти новые запросы:
                    // нужен только последний.
                    while let Ok(newer) = requests.try_recv() {
                        request = newer;
                    }
                    let stale = || !requests.is_empty();
                    // Ошибка разбора одного необычного файла не должна убивать
                    // поток: тогда все следующие просмотры зависли бы в загрузке.
                    let preview = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        build(&request.1, &stale)
                    }))
                    .unwrap_or_else(|_| Preview {
                        path: request.1.clone(),
                        size: 0,
                        modified: None,
                        mode: String::new(),
                        owner: String::new(),
                        body: Body::Error("internal error while reading the file".into()),
                    });
                    if results.send((request.0, preview)).is_err() {
                        break;
                    }
                }
            });
        // Без потока просмотр просто останется пустым; падать из-за этого незачем.
        drop(spawned);
        Self { tx, rx, next: 0 }
    }

    /// Ставит путь в очередь; возвращает номер, с которым придёт результат.
    pub fn request(&mut self, path: PathBuf) -> u64 {
        self.next += 1;
        let _ = self.tx.send((self.next, path));
        self.next
    }
}

/// Строит просмотр пути. `stale` сообщает, что результат уже не нужен.
pub fn build(path: &Path, stale: &dyn Fn() -> bool) -> Preview {
    let meta = fs::metadata(path).or_else(|_| fs::symlink_metadata(path));
    let (size, modified, mode, owner) = match &meta {
        Ok(meta) => (
            meta.len(),
            meta.modified().ok(),
            mode_string(meta),
            owner_string(meta),
        ),
        Err(_) => (0, None, String::new(), String::new()),
    };
    let body = match meta {
        Err(err) => Body::Error(err.to_string()),
        Ok(meta) if meta.is_dir() => Body::Dir(dir_info(path, stale)),
        Ok(meta) => file_body(path, &meta),
    };
    Preview {
        path: path.to_path_buf(),
        size,
        modified,
        mode,
        owner,
        body,
    }
}

fn file_body(path: &Path, meta: &Metadata) -> Body {
    if !meta.is_file() {
        return Body::Binary(Vec::new());
    }
    if crate::archive::Format::detect(path).is_some() {
        return match crate::archive::list(path) {
            Ok(items) => Body::Archive(items),
            Err(err) => Body::Error(err.to_string()),
        };
    }
    let mut bytes = Vec::new();
    let read = File::open(path).and_then(|f| f.take(TEXT_LIMIT).read_to_end(&mut bytes));
    if let Err(err) = read {
        return Body::Error(err.to_string());
    }
    if !looks_textual(&bytes[..bytes.len().min(SNIFF)]) {
        return Body::Binary(hex_dump(&bytes[..bytes.len().min(HEX_LIMIT)]));
    }
    Body::Text(text_body(
        path,
        &String::from_utf8_lossy(&bytes),
        meta.len() > TEXT_LIMIT,
    ))
}

/// Разбирает текст на строки и подсвечивает их.
pub fn text_body(path: &Path, text: &str, truncated: bool) -> TextBody {
    let mut raw: Vec<&str> = text.lines().collect();
    let truncated = truncated || raw.len() > LINE_LIMIT;
    raw.truncate(LINE_LIMIT);

    let sample: Vec<&str> = raw.iter().take(30).copied().collect();
    let language = highlight::detect_text(path, &sample);
    let mut highlighter = Highlighter::new(language);

    let markdown = crate::markdown::applies(path).then(|| crate::markdown::render(&raw));
    let mut lines = Vec::with_capacity(raw.len());
    let mut widths = Vec::with_capacity(raw.len());
    for line in raw {
        let line = expand_tabs(line.strip_suffix('\r').unwrap_or(line));
        widths.push(line.width());
        lines.push(highlighter.line(&line));
    }
    TextBody {
        lines,
        widths,
        language: language.map(|l| l.name),
        numbered: language.is_some_and(|l| l.numbered()),
        truncated,
        markdown,
    }
}

/// Текст ли это: нет нулевых байт и почти нет управляющих символов.
fn looks_textual(head: &[u8]) -> bool {
    if head.contains(&0) {
        return false;
    }
    let control = head
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0c | 0x1b))
        .count();
    control * 100 <= head.len()
}

/// Раскрывает табуляцию с учётом позиции: колонки выравниваются как в редакторе.
fn expand_tabs(line: &str) -> String {
    if !line.contains('\t') {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len() + 8);
    let mut col = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let n = TAB_WIDTH - col % TAB_WIDTH;
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(ch);
            col += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        }
    }
    out
}

/// Классический дамп: смещение, 16 байт в hex и они же символами.
fn hex_dump(bytes: &[u8]) -> Vec<String> {
    bytes
        .chunks(16)
        .enumerate()
        .map(|(i, chunk)| {
            let mut hex = String::with_capacity(16 * 3 + 1);
            for (j, b) in chunk.iter().enumerate() {
                if j == 8 {
                    hex.push(' ');
                }
                hex.push_str(&format!("{b:02x} "));
            }
            let ascii: String = chunk
                .iter()
                .map(|&b| {
                    if b.is_ascii_graphic() || b == b' ' {
                        b as char
                    } else {
                        '.'
                    }
                })
                .collect();
            format!("{:08x}  {hex:<49} |{ascii}|", i * 16)
        })
        .collect()
}

/// Собирает сведения о директории, считая размер всего поддерева.
fn dir_info(path: &Path, stale: &dyn Fn() -> bool) -> DirInfo {
    let mut info = DirInfo {
        complete: true,
        ..DirInfo::default()
    };
    let Ok(children) = fs::read_dir(path) else {
        info.complete = false;
        return info;
    };
    let deadline = Instant::now() + DIR_TIME_LIMIT;
    let mut sizes = Vec::new();
    for child in children.flatten() {
        let name = child.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            info.hidden += 1;
        }
        let Ok(meta) = fs::symlink_metadata(child.path()) else {
            continue;
        };
        let is_dir = meta.is_dir();
        if meta.file_type().is_symlink() {
            info.links += 1;
        } else if is_dir {
            info.dirs += 1;
        } else {
            info.files += 1;
        }
        let (bytes, items) = if is_dir {
            let mut acc = (0, 0);
            if !tree_size(&child.path(), &mut acc, deadline, stale) {
                info.complete = false;
            }
            acc
        } else {
            (meta.len(), 0)
        };
        info.total_size += bytes;
        info.total_items += items + 1;
        sizes.push((name, bytes, is_dir));
    }
    sizes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    sizes.truncate(LARGEST);
    info.largest = sizes;
    info
}

/// Добавляет к `acc` байты и число элементов поддерева. `false` — подсчёт
/// прерван по времени или потому, что результат больше не нужен.
fn tree_size(
    path: &Path,
    acc: &mut (u64, u64),
    deadline: Instant,
    stale: &dyn Fn() -> bool,
) -> bool {
    let Ok(children) = fs::read_dir(path) else {
        return true;
    };
    for child in children.flatten() {
        if Instant::now() > deadline || stale() {
            return false;
        }
        let Ok(meta) = fs::symlink_metadata(child.path()) else {
            continue;
        };
        acc.1 += 1;
        if meta.is_dir() {
            if !tree_size(&child.path(), acc, deadline, stale) {
                return false;
            }
        } else {
            acc.0 += meta.len();
        }
    }
    true
}

#[cfg(unix)]
fn mode_string(meta: &Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    let mode = meta.permissions().mode();
    let kind = if meta.is_dir() {
        'd'
    } else if meta.file_type().is_symlink() {
        'l'
    } else {
        '-'
    };
    let mut out = String::from(kind);
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        out.push(if bits & 4 != 0 { 'r' } else { '-' });
        out.push(if bits & 2 != 0 { 'w' } else { '-' });
        out.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    out
}

#[cfg(not(unix))]
fn mode_string(meta: &Metadata) -> String {
    if meta.permissions().readonly() {
        "readonly".into()
    } else {
        String::new()
    }
}

#[cfg(unix)]
fn owner_string(meta: &Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    format!("{}:{}", meta.uid(), meta.gid())
}

#[cfg(not(unix))]
fn owner_string(_meta: &Metadata) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn never() -> bool {
        false
    }

    #[test]
    fn text_file_is_highlighted_and_numbered() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("main.rs");
        fs::write(&path, "fn main() {\r\n\tlet x = 1;\r\n}\r\n").unwrap();
        let preview = build(&path, &never);
        let Body::Text(text) = preview.body else {
            panic!("ожидался текст");
        };
        assert_eq!(text.language, Some("Rust"));
        assert!(text.numbered);
        assert_eq!(text.lines.len(), 3);
        // Табуляция раскрыта, перевод строки Windows срезан.
        assert_eq!(text.plain(1), "    let x = 1;");
        assert_eq!(text.widths[1], 14);
        assert_eq!(preview.size, 29);
    }

    #[test]
    fn plain_text_has_no_numbers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("notes.txt");
        fs::write(&path, "hello\nworld").unwrap();
        let Body::Text(text) = build(&path, &never).body else {
            panic!();
        };
        assert!(text.language.is_none());
        assert!(!text.numbered);
        assert!(text.markdown.is_none());
    }

    #[test]
    fn markdown_gets_rendered_view() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("README.md");
        fs::write(&path, "# Title\n\n**bold** text\n").unwrap();
        let Body::Text(text) = build(&path, &never).body else {
            panic!();
        };
        let doc = text.markdown.expect("ожидалось оформление");
        assert_eq!(doc.lines[0].plain(), "Title");
        assert_eq!(doc.lines.last().unwrap().plain(), "bold text");
    }

    #[test]
    fn binary_file_becomes_hex_dump() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.bin");
        let bytes: Vec<u8> = (0u8..40).collect();
        fs::write(&path, &bytes).unwrap();
        let Body::Binary(lines) = build(&path, &never).body else {
            panic!("ожидался дамп");
        };
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with("00000000  00 01 02"));
        assert!(lines[2].starts_with("00000020  20 21"));
        assert!(lines[2].ends_with("| !\"#$%&'|"));
    }

    #[test]
    fn directory_info_counts_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("big/inner")).unwrap();
        fs::write(root.join("big/inner/data"), vec![0u8; 1000]).unwrap();
        fs::write(root.join("small"), "abc").unwrap();
        fs::write(root.join(".hidden"), "").unwrap();

        let preview = build(root, &never);
        let Body::Dir(info) = preview.body else {
            panic!("ожидалась директория");
        };
        assert!(info.complete);
        assert_eq!((info.files, info.dirs, info.hidden), (2, 1, 1));
        assert_eq!(info.total_size, 1003);
        // big, big/inner, big/inner/data, small, .hidden
        assert_eq!(info.total_items, 5);
        assert_eq!(info.largest[0], ("big".to_string(), 1000, true));
        assert_eq!(info.largest[1].0, "small");
        #[cfg(unix)]
        assert!(preview.mode.starts_with('d'));
    }

    #[test]
    fn stale_request_aborts_directory_count() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("a/b")).unwrap();
        fs::write(tmp.path().join("a/b/c"), "x").unwrap();
        let Body::Dir(info) = build(tmp.path(), &|| true).body else {
            panic!();
        };
        assert!(!info.complete);
    }

    #[test]
    fn missing_path_is_an_error() {
        let preview = build(Path::new("/definitely/not/here"), &never);
        assert!(matches!(preview.body, Body::Error(_)));
    }

    #[test]
    fn loader_returns_latest_result() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let mut loader = Loader::new();
        let id = loader.request(tmp.path().join("a.txt"));
        let (got, preview) = loader.rx.recv().unwrap();
        assert_eq!(got, id);
        assert_eq!(preview.size, 1);
    }

    #[test]
    fn tabs_expand_to_next_stop() {
        assert_eq!(expand_tabs("a\tb"), "a   b");
        assert_eq!(expand_tabs("\t\tx"), "        x");
        assert_eq!(expand_tabs("abcd\tx"), "abcd    x");
    }
}
