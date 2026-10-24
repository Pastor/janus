//! Оформленный показ Markdown: заголовки без решёток, списки с маркерами,
//! таблицы с рамками, выделение и код вместо звёздочек и обратных кавычек.
//!
//! Это не полный CommonMark, а разбор того, что встречается в README и
//! заметках. Абзацы склеиваются в одну логическую строку: на экранные строки
//! по словам её режет отрисовка, когда известна ширина панели. Каждая строка
//! помнит, из какой строки исходника она получилась, — так при переключении
//! между оформлением и исходником остаётся видно то же место.

use std::path::Path;
use std::sync::Arc;

use unicode_width::UnicodeWidthStr;

use crate::highlight::{self, Highlighter, Token};

/// Расширения, для которых доступно оформление.
const EXTENSIONS: &[&str] = &["md", "markdown", "mdown", "mkd", "mkdn"];

/// Маркеры пунктов по уровню вложенности.
const BULLETS: &[&str] = &["•", "◦", "▪"];

/// Отметка цитаты; повторяется по уровню вложенности.
const QUOTE_BAR: &str = "▎ ";

/// Отступ строк кода.
const CODE_INDENT: &str = "  ";

/// Можно ли показывать файл оформленным.
pub fn applies(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Роль фрагмента; цвет ей назначает тема.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Plain,
    /// Заголовок уровня 1–6.
    Heading(u8),
    /// Код в строке или блоке без известного языка.
    Code,
    Link,
    Quote,
    /// Служебные знаки: маркеры списков, рамки таблиц, линии.
    Marker,
    /// Подсвеченный код из блока с известным языком.
    Syntax(Token),
}

/// Фрагмент строки одного оформления.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub text: String,
    pub kind: Kind,
    pub strong: bool,
    pub emph: bool,
    pub strike: bool,
}

/// Строка оформленного документа.
#[derive(Debug, Clone, Default)]
pub struct Line {
    pub segs: Vec<Seg>,
    /// Что рисовать в начале строк, на которые она перенесётся: отступ
    /// пункта списка, отметки цитаты.
    pub cont: Vec<Seg>,
    /// Линия во всю ширину этим символом (после `segs`).
    pub rule: Option<char>,
    /// Строка исходника, из которой получена эта.
    pub source: usize,
    /// Строка таблицы; `segs` тогда — только отметки цитаты перед ней.
    pub row: Option<Row>,
}

/// Строка таблицы. Ширину колонок под ширину панели подбирает отрисовка.
#[derive(Debug, Clone)]
pub struct Row {
    /// Ячейки; пусто у черты под заголовком.
    pub cells: Vec<Vec<Seg>>,
    pub table: Arc<Table>,
}

/// Общее для всех строк таблицы.
#[derive(Debug)]
pub struct Table {
    /// Ширина самой широкой ячейки каждой колонки.
    pub widths: Vec<usize>,
    pub aligns: Vec<Align>,
}

/// Разделитель колонок таблицы.
pub const COLUMN_GAP: &str = " │ ";

impl Line {
    /// Текст строки без оформления.
    pub fn plain(&self) -> String {
        let mut out: String = self.segs.iter().map(|s| s.text.as_str()).collect();
        if let Some(row) = &self.row {
            out.push_str(&row.plain_cells().join(COLUMN_GAP));
        }
        out
    }

    fn is_blank(&self) -> bool {
        self.rule.is_none()
            && self.row.is_none()
            && self.segs.iter().all(|s| s.text.trim().is_empty())
    }
}

impl Row {
    /// Текст ячеек без оформления.
    pub fn plain_cells(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|cell| cell.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }
}

/// Оформленный документ.
#[derive(Debug, Clone, Default)]
pub struct Doc {
    pub lines: Vec<Line>,
}

impl Doc {
    /// Первая строка документа, полученная из строки исходника `source` или
    /// из следующих за ней.
    pub fn line_of(&self, source: usize) -> usize {
        self.lines
            .partition_point(|l| l.source < source)
            .min(self.lines.len().saturating_sub(1))
    }

    /// Строка исходника, из которой получена строка документа `line`.
    pub fn to_source(&self, line: usize) -> usize {
        self.lines.get(line).map_or(0, |l| l.source)
    }
}

/// Стиль выделения внутри строки.
#[derive(Debug, Clone, Copy, Default)]
struct Flags {
    strong: bool,
    emph: bool,
    strike: bool,
}

/// Открытый блок кода.
struct Fence {
    marker: char,
    len: usize,
    highlighter: Highlighter,
    /// Глубина цитаты, в которой блок открыт.
    quotes: usize,
    prefix: Vec<Seg>,
}

/// Копящийся абзац: куски между жёсткими переносами и их исходные строки.
struct Para {
    parts: Vec<(String, usize)>,
    prefix: Vec<Seg>,
    cont: Vec<Seg>,
    base: Kind,
    /// Предыдущая строка кончилась жёстким переносом.
    broken: bool,
}

/// Разбирает исходник в оформленный документ.
pub fn render(source: &[&str]) -> Doc {
    let mut b = Builder::default();
    let mut i = 0;
    while i < source.len() {
        i = b.line(source, i);
    }
    b.flush();
    while b.out.last().is_some_and(Line::is_blank) {
        b.out.pop();
    }
    Doc { lines: b.out }
}

#[derive(Default)]
struct Builder {
    out: Vec<Line>,
    para: Option<Para>,
    fence: Option<Fence>,
    /// Отступы маркеров открытых списков, от внешнего к внутреннему.
    lists: Vec<usize>,
}

impl Builder {
    /// Разбирает строку `i` (или несколько, как таблица); возвращает
    /// номер следующей неразобранной.
    fn line(&mut self, source: &[&str], i: usize) -> usize {
        let raw = source[i].strip_suffix('\r').unwrap_or(source[i]);
        let (quotes, rest) = strip_quotes(raw);
        let prefix = quote_prefix(quotes);

        if let Some(fence) = &mut self.fence {
            let inner = if quotes >= fence.quotes { rest } else { raw };
            let trimmed = inner.trim_start();
            if closes_fence(trimmed, fence.marker, fence.len) {
                self.fence = None;
            } else {
                let mut segs = fence.prefix.clone();
                segs.push(seg(CODE_INDENT, Kind::Code));
                for piece in fence.highlighter.line(&expand(inner)) {
                    let kind = match piece.kind {
                        Token::Plain => Kind::Code,
                        token => Kind::Syntax(token),
                    };
                    segs.push(seg(&piece.text, kind));
                }
                let mut cont = fence.prefix.clone();
                cont.push(seg(CODE_INDENT, Kind::Code));
                self.push(Line {
                    segs,
                    cont,
                    rule: None,
                    row: None,
                    source: i,
                });
            }
            return i + 1;
        }

        let text = expand(rest);
        let indent = text.len() - text.trim_start().len();
        let body = text.trim_start();

        if body.is_empty() {
            self.flush();
            self.blank(i);
            return i + 1;
        }

        // Ленивое продолжение абзаца — пока строка не начинает новый блок.
        let starts_block = starts_block(body) || list_item(body).is_some();
        if let Some(para) = &mut self.para
            && !starts_block
        {
            if let Some(level) = setext(body)
                && para.parts.len() == 1
                && para.base == Kind::Plain
                && self.lists.is_empty()
            {
                let para = self.para.take().unwrap();
                let (text, line) = para.parts.into_iter().next().unwrap();
                self.heading(&prefix, level, &text, line);
                return i + 1;
            }
            let (chunk, hard) = hard_break(body);
            if para.broken {
                para.parts.push((chunk.to_string(), i));
            } else if let Some(last) = para.parts.last_mut() {
                last.0.push(' ');
                last.0.push_str(chunk);
            }
            para.broken = hard;
            return i + 1;
        }

        if let Some((marker, len, lang)) = opens_fence(body) {
            self.flush();
            self.fence = Some(Fence {
                marker,
                len,
                highlighter: Highlighter::new(fence_language(lang)),
                quotes,
                prefix: prefix.clone(),
            });
            return i + 1;
        }

        if let Some((level, title)) = atx(body).or_else(|| html_heading(body)) {
            self.flush();
            self.lists.clear();
            self.heading(&prefix, level, &title, i);
            return i + 1;
        }

        if is_rule(body) {
            self.flush();
            self.lists.clear();
            self.push(Line {
                segs: prefix.clone(),
                cont: prefix,
                rule: Some('─'),
                row: None,
                source: i,
            });
            return i + 1;
        }

        if body.contains('|')
            && let Some(next) = source.get(i + 1)
            && let Some(aligns) = table_separator(strip_quotes(next).1)
        {
            self.flush();
            return self.table(source, i, quotes, &prefix, aligns);
        }

        if let Some((marker, content)) = list_item(body) {
            self.flush();
            while self.lists.last().is_some_and(|&l| l > indent) {
                self.lists.pop();
            }
            if self.lists.last() != Some(&indent) {
                self.lists.push(indent);
            }
            let level = self.lists.len() - 1;
            let pad = "  ".repeat(level);
            let (mark, content) = match task(content) {
                Some((done, rest)) => (if done { "☑" } else { "☐" }.to_string(), rest),
                None if marker.ends_with(['.', ')']) => (marker.to_string(), content),
                None => (BULLETS[level % BULLETS.len()].to_string(), content),
            };
            let mut first = prefix.clone();
            first.push(seg(&format!("{pad}{mark} "), Kind::Marker));
            let mut cont = prefix.clone();
            cont.push(seg(
                &" ".repeat(pad.width() + mark.width() + 1),
                Kind::Plain,
            ));
            self.start_para(content, i, first, cont, Kind::Plain);
            return i + 1;
        }

        // Абзац внутри пункта списка: отступ как у текста пункта.
        let depth = self.lists.iter().filter(|&&l| l < indent).count();
        if depth > 0 {
            self.lists.truncate(depth);
            let mut first = prefix.clone();
            first.push(seg(&" ".repeat(2 * depth), Kind::Plain));
            let cont = first.clone();
            self.start_para(body, i, first, cont, Kind::Plain);
            return i + 1;
        }
        self.lists.clear();

        if indent >= 4 {
            let mut segs = prefix.clone();
            segs.push(seg(&format!("{CODE_INDENT}{}", &text[4..]), Kind::Code));
            let mut cont = prefix;
            cont.push(seg(CODE_INDENT, Kind::Code));
            self.push(Line {
                segs,
                cont,
                rule: None,
                row: None,
                source: i,
            });
            return i + 1;
        }

        let base = if quotes > 0 { Kind::Quote } else { Kind::Plain };
        let cont = prefix.clone();
        self.start_para(body, i, prefix, cont, base);
        i + 1
    }

    fn start_para(
        &mut self,
        text: &str,
        line: usize,
        prefix: Vec<Seg>,
        cont: Vec<Seg>,
        base: Kind,
    ) {
        let (chunk, hard) = hard_break(text);
        self.para = Some(Para {
            parts: vec![(chunk.to_string(), line)],
            prefix,
            cont,
            base,
            broken: hard,
        });
    }

    /// Выводит накопленный абзац.
    fn flush(&mut self) {
        let Some(para) = self.para.take() else {
            return;
        };
        let mut first = true;
        for (text, source) in para.parts {
            let mut segs = if first {
                para.prefix.clone()
            } else {
                para.cont.clone()
            };
            first = false;
            let mut content = Vec::new();
            inline(&text, para.base, Flags::default(), &mut content);
            // На месте снятых тегов HTML остаются пробелы по краям.
            if let Some(s) = content.first_mut() {
                s.text = s.text.trim_start().to_string();
            }
            if let Some(s) = content.last_mut() {
                s.text = s.text.trim_end().to_string();
            }
            content.retain(|s| !s.text.is_empty());
            // Строка из одних тегов не оставляет ничего.
            if content.is_empty() {
                continue;
            }
            segs.extend(content);
            self.push(Line {
                segs,
                cont: para.cont.clone(),
                rule: None,
                row: None,
                source,
            });
        }
    }

    fn heading(&mut self, prefix: &[Seg], level: u8, title: &str, source: usize) {
        if self.out.last().is_some_and(|l| !l.is_blank()) {
            self.blank(source);
        }
        let mut segs = prefix.to_vec();
        inline(
            title.trim(),
            Kind::Heading(level),
            Flags::default(),
            &mut segs,
        );
        self.push(Line {
            segs,
            cont: prefix.to_vec(),
            rule: None,
            row: None,
            source,
        });
        let rule = match level {
            1 => Some('═'),
            2 => Some('─'),
            _ => None,
        };
        if rule.is_some() {
            self.push(Line {
                segs: prefix.to_vec(),
                cont: prefix.to_vec(),
                rule,
                source,
                row: None,
            });
        }
    }

    /// Таблица: строки до первой без `|`. Колонки выравниваются по самой
    /// широкой ячейке.
    fn table(
        &mut self,
        source: &[&str],
        start: usize,
        quotes: usize,
        prefix: &[Seg],
        aligns: Vec<Align>,
    ) -> usize {
        let mut rows: Vec<(usize, Vec<Vec<Seg>>)> = Vec::new();
        let mut i = start;
        while i < source.len() {
            let (q, rest) = strip_quotes(source[i]);
            let rest = rest.trim();
            if q != quotes || rest.is_empty() || !rest.contains('|') {
                break;
            }
            if i != start + 1 {
                let flags = Flags {
                    strong: i == start,
                    ..Flags::default()
                };
                let cells = split_row(rest)
                    .into_iter()
                    .map(|cell| {
                        let mut segs = Vec::new();
                        inline(cell.trim(), Kind::Plain, flags, &mut segs);
                        segs
                    })
                    .collect();
                rows.push((i, cells));
            }
            i += 1;
        }
        let columns = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
        let mut widths = vec![0; columns];
        for (_, cells) in &mut rows {
            cells.resize_with(columns, Vec::new);
            for (w, cell) in widths.iter_mut().zip(cells.iter()) {
                *w = (*w).max(segs_width(cell));
            }
        }
        let table = Arc::new(Table { widths, aligns });
        for (n, (line, cells)) in rows.into_iter().enumerate() {
            let row = |cells, source| Line {
                segs: prefix.to_vec(),
                cont: prefix.to_vec(),
                rule: None,
                source,
                row: Some(Row {
                    cells,
                    table: table.clone(),
                }),
            };
            self.push(row(cells, line));
            if n == 0 {
                self.push(row(Vec::new(), start + 1));
            }
        }
        i
    }

    /// Пустая строка; подряд их не бывает, и документ с них не начинается.
    fn blank(&mut self, source: usize) {
        if self.out.last().is_some_and(|l| !l.is_blank()) {
            self.out.push(Line {
                source,
                ..Line::default()
            });
        }
    }

    fn push(&mut self, line: Line) {
        self.out.push(line);
    }
}

fn seg(text: &str, kind: Kind) -> Seg {
    Seg {
        text: text.to_string(),
        kind,
        strong: false,
        emph: false,
        strike: false,
    }
}

fn segs_width(segs: &[Seg]) -> usize {
    segs.iter().map(|s| s.text.width()).sum()
}

/// Табуляция в пробелы: отступы сравниваются в колонках.
fn expand(text: &str) -> String {
    text.replace('\t', "    ")
}

/// Снимает отметки цитаты `>`; возвращает их число и остаток строки.
fn strip_quotes(line: &str) -> (usize, &str) {
    let mut rest = line;
    let mut depth = 0;
    loop {
        let trimmed = rest.trim_start_matches(' ');
        if rest.len() - trimmed.len() > 3 {
            break;
        }
        let Some(after) = trimmed.strip_prefix('>') else {
            break;
        };
        depth += 1;
        rest = after.strip_prefix(' ').unwrap_or(after);
    }
    (depth, rest)
}

fn quote_prefix(depth: usize) -> Vec<Seg> {
    (0..depth).map(|_| seg(QUOTE_BAR, Kind::Quote)).collect()
}

/// Строка начинает блок, прерывающий абзац.
fn starts_block(body: &str) -> bool {
    opens_fence(body).is_some()
        || atx(body).is_some()
        || html_heading(body).is_some()
        || (is_rule(body) && setext(body).is_none())
}

/// Открывающая ограда блока кода: символ, длина и язык.
fn opens_fence(body: &str) -> Option<(char, usize, &str)> {
    let marker = body.chars().next().filter(|c| matches!(c, '`' | '~'))?;
    let len = body.chars().take_while(|&c| c == marker).count();
    if len < 3 {
        return None;
    }
    let info = body[len..].trim();
    if marker == '`' && info.contains('`') {
        return None;
    }
    Some((marker, len, info.split_whitespace().next().unwrap_or("")))
}

fn closes_fence(body: &str, marker: char, len: usize) -> bool {
    let n = body.chars().take_while(|&c| c == marker).count();
    n >= len && body[n..].trim().is_empty()
}

/// Язык блока кода по слову после ограды: `rust`, `rs`, `sh`, `toml`…
fn fence_language(info: &str) -> Option<&'static highlight::Language> {
    if info.is_empty() {
        return None;
    }
    let info = info.to_ascii_lowercase();
    let ext = match info.as_str() {
        "rust" => "rs",
        "python" => "py",
        "bash" | "shell" | "zsh" | "console" => "sh",
        "javascript" => "js",
        "typescript" => "ts",
        "yaml" => "yml",
        "golang" => "go",
        "c++" => "cpp",
        "csharp" => "cs",
        "kotlin" => "kt",
        "ruby" => "rb",
        other => other,
    };
    highlight::detect(Path::new(&format!("x.{ext}")), "")
}

/// Заголовок `## Текст`.
fn atx(body: &str) -> Option<(u8, String)> {
    let level = body.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &body[level..];
    if !rest.is_empty() && !rest.starts_with(' ') {
        return None;
    }
    // Закрывающие решётки необязательны.
    let title = rest.trim().trim_end_matches('#').trim_end();
    Some((level as u8, title.to_string()))
}

/// Заголовок, записанный тегом: `<h1 align="center">Janus</h1>`.
fn html_heading(body: &str) -> Option<(u8, String)> {
    let rest = body
        .strip_prefix("<h")
        .or_else(|| body.strip_prefix("<H"))?;
    let level = rest.chars().next()?.to_digit(10)?;
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[1..];
    if !after.starts_with(['>', ' ']) {
        return None;
    }
    let close = after.find('>')?;
    let title = after[close + 1..].split("</").next().unwrap_or("");
    Some((level as u8, title.to_string()))
}

/// Подчёркивание заголовка: `===` — первый уровень, `---` — второй.
fn setext(body: &str) -> Option<u8> {
    let body = body.trim_end();
    if body.chars().all(|c| c == '=') {
        Some(1)
    } else if body.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

/// Горизонтальная линия: три и больше `-`, `*` или `_`, можно через пробел.
fn is_rule(body: &str) -> bool {
    let mut chars = body.chars().filter(|c| !c.is_whitespace());
    let Some(first) = chars.next().filter(|c| matches!(c, '-' | '*' | '_')) else {
        return false;
    };
    let mut n = 1;
    for c in chars {
        if c != first {
            return false;
        }
        n += 1;
    }
    n >= 3
}

/// Пункт списка: маркер (`-`, `*`, `+`, `1.`, `1)`) и текст после него.
fn list_item(body: &str) -> Option<(&str, &str)> {
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    let end = if digits > 0 {
        if digits > 9 || !body[digits..].starts_with(['.', ')']) {
            return None;
        }
        digits + 1
    } else if body.starts_with(['-', '*', '+']) {
        1
    } else {
        return None;
    };
    let rest = &body[end..];
    if rest.is_empty() {
        return Some((&body[..end], ""));
    }
    let content = rest.strip_prefix(' ')?;
    if is_rule(body) {
        return None;
    }
    Some((&body[..end], content.trim_start()))
}

/// Отметка задачи `[ ]` / `[x]` в начале пункта.
fn task(content: &str) -> Option<(bool, &str)> {
    let done = match content.get(..3)? {
        "[ ]" => false,
        "[x]" | "[X]" => true,
        _ => return None,
    };
    let rest = &content[3..];
    (rest.is_empty() || rest.starts_with(' ')).then(|| (done, rest.trim_start()))
}

/// Конец строки: два пробела или `\` означают жёсткий перенос.
fn hard_break(text: &str) -> (&str, bool) {
    if let Some(stripped) = text.strip_suffix('\\') {
        return (stripped, true);
    }
    let trimmed = text.trim_end();
    (trimmed, text.ends_with("  "))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Вторая строка таблицы: `|---|:--:|--:|`.
fn table_separator(line: &str) -> Option<Vec<Align>> {
    let line = line.trim();
    if !line.contains('-') || !line.contains(['|', ':']) {
        return None;
    }
    let cells = split_row(line);
    let mut aligns = Vec::with_capacity(cells.len());
    for cell in cells {
        let cell = cell.trim();
        let core = cell.trim_matches(':');
        if core.is_empty() || !core.chars().all(|c| c == '-') {
            return None;
        }
        aligns.push(match (cell.starts_with(':'), cell.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    Some(aligns)
}

/// Ячейки строки таблицы; `\|` — черта внутри ячейки.
fn split_row(line: &str) -> Vec<String> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = if line.ends_with('|') && !line.ends_with("\\|") {
        &line[..line.len() - 1]
    } else {
        line
    };
    let mut cells = vec![String::new()];
    let mut chars = line.chars().peekable();
    let mut in_code = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cells.last_mut().unwrap().push('|');
                chars.next();
            }
            '`' => {
                in_code = !in_code;
                cells.last_mut().unwrap().push(c);
            }
            '|' if !in_code => cells.push(String::new()),
            _ => cells.last_mut().unwrap().push(c),
        }
    }
    cells
}

/// Добавляет к `out` фрагмент, склеивая его с предыдущим того же вида.
fn push_text(out: &mut Vec<Seg>, text: &str, kind: Kind, flags: Flags) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.kind == kind
        && last.strong == flags.strong
        && last.emph == flags.emph
        && last.strike == flags.strike
    {
        last.text.push_str(text);
        return;
    }
    out.push(Seg {
        text: text.to_string(),
        kind,
        strong: flags.strong,
        emph: flags.emph,
        strike: flags.strike,
    });
}

/// Разметка внутри строки: выделение, код, ссылки, сущности, теги.
fn inline(text: &str, base: Kind, flags: Flags, out: &mut Vec<Seg>) {
    let chars: Vec<char> = text.chars().collect();
    let mut plain = String::new();
    let mut i = 0;
    macro_rules! emit {
        () => {
            push_text(out, &std::mem::take(&mut plain), base, flags)
        };
    }
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\\' if next.is_some_and(|n| n.is_ascii_punctuation()) => {
                plain.push(next.unwrap());
                i += 2;
            }
            '`' => {
                let n = run(&chars, i, '`');
                match find_run(&chars, i + n, '`', n) {
                    Some(end) => {
                        emit!();
                        let code: String = chars[i + n..end].iter().collect();
                        let code = code.strip_prefix(' ').unwrap_or(&code);
                        let code = code.strip_suffix(' ').unwrap_or(code);
                        push_text(out, code, Kind::Code, flags);
                        i = end + n;
                    }
                    None => {
                        plain.extend(&chars[i..i + n]);
                        i += n;
                    }
                }
            }
            '*' | '_' | '~' => {
                let n = run(&chars, i, c);
                let width = match c {
                    '~' if n >= 2 => 2,
                    '~' => 0,
                    _ if n >= 2 => 2,
                    _ => 1,
                };
                let closer = (width > 0)
                    .then(|| emphasis_end(&chars, i, c, width))
                    .flatten();
                match closer {
                    Some(end) => {
                        emit!();
                        let inner: String = chars[i + width..end].iter().collect();
                        let mut f = flags;
                        match (c, width) {
                            ('~', _) => f.strike = true,
                            (_, 2) => f.strong = true,
                            _ => f.emph = true,
                        }
                        inline(&inner, base, f, out);
                        i = end + width;
                    }
                    None => {
                        plain.extend(&chars[i..i + n]);
                        i += n;
                    }
                }
            }
            '!' if next == Some('[') => match link(&chars, i + 1) {
                Some((label, end)) => {
                    emit!();
                    let label = if label.is_empty() {
                        "image".into()
                    } else {
                        label
                    };
                    push_text(out, &format!("[{label}]"), Kind::Link, flags);
                    i = end;
                }
                None => {
                    plain.push(c);
                    i += 1;
                }
            },
            '[' => match link(&chars, i) {
                Some((label, end)) => {
                    emit!();
                    inline(&label, Kind::Link, flags, out);
                    i = end;
                }
                None => {
                    plain.push(c);
                    i += 1;
                }
            },
            '<' => match tag(&chars, i) {
                Some((Tag::Link(url), end)) => {
                    emit!();
                    push_text(out, &url, Kind::Link, flags);
                    i = end;
                }
                Some((Tag::Image(alt), end)) => {
                    emit!();
                    push_text(out, &format!("[{alt}]"), Kind::Link, flags);
                    i = end;
                }
                Some((Tag::Other, end)) => i = end,
                None => {
                    plain.push(c);
                    i += 1;
                }
            },
            '&' => match entity(&chars, i) {
                Some((ch, end)) => {
                    plain.push(ch);
                    i = end;
                }
                None => {
                    plain.push(c);
                    i += 1;
                }
            },
            _ => {
                plain.push(c);
                i += 1;
            }
        }
    }
    emit!();
}

/// Длина серии символов `c`, начиная с `i`.
fn run(chars: &[char], i: usize, c: char) -> usize {
    chars[i..].iter().take_while(|&&x| x == c).count()
}

/// Начало ближайшей серии ровно из `n` символов `c` после `from`.
fn find_run(chars: &[char], from: usize, c: char, n: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        if chars[i] == c {
            let len = run(chars, i, c);
            if len == n {
                return Some(i);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

/// Где закрывается выделение, открытое в `i` разделителем из `width`
/// символов `c`. Открывающий разделитель должен прилегать к тексту справа,
/// закрывающий — слева; `_` внутри слова (`snake_case`) не выделяет.
fn emphasis_end(chars: &[char], i: usize, c: char, width: usize) -> Option<usize> {
    let after = *chars.get(i + width)?;
    if after.is_whitespace() {
        return None;
    }
    let word = |x: Option<&char>| x.is_some_and(|x| x.is_alphanumeric());
    if c == '_' && i > 0 && word(chars.get(i - 1)) {
        return None;
    }
    let mut j = i + width + 1;
    while j + width <= chars.len() {
        if chars[j] == '`' {
            // Код внутри выделения не содержит его разделителей.
            let n = run(chars, j, '`');
            j = find_run(chars, j + n, '`', n).map_or(j + n, |e| e + n);
            continue;
        }
        if chars[j..j + width].iter().all(|&x| x == c)
            && !chars[j - 1].is_whitespace()
            && (width == 2 || chars.get(j + 1) != Some(&c))
            && !(c == '_' && word(chars.get(j + width)))
        {
            return Some(j);
        }
        j += 1;
    }
    None
}

/// Ссылка `[текст](адрес)` или `[текст][метка]`, начинающаяся в `i`:
/// текст и позиция после неё.
fn link(chars: &[char], i: usize) -> Option<(String, usize)> {
    let mut depth = 0;
    let mut j = i;
    let close = loop {
        match chars.get(j)? {
            '\\' => j += 1,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    break j;
                }
            }
            _ => {}
        }
        j += 1;
    };
    let label: String = chars[i + 1..close].iter().collect();
    let (open, end) = match chars.get(close + 1) {
        Some('(') => ('(', ')'),
        Some('[') => ('[', ']'),
        _ => return None,
    };
    let _ = open;
    let mut depth = 0;
    let mut k = close + 1;
    loop {
        let c = *chars.get(k)?;
        if c == '(' && end == ')' {
            depth += 1;
        } else if c == end {
            depth -= 1;
            if depth <= 0 {
                break;
            }
        }
        k += 1;
    }
    Some((label, k + 1))
}

enum Tag {
    /// Автоссылка `<https://…>`.
    Link(String),
    /// `<img alt="…">`.
    Image(String),
    /// Любой другой тег: не показывается.
    Other,
}

/// Тег HTML или автоссылка, начинающиеся в `i`.
fn tag(chars: &[char], i: usize) -> Option<(Tag, usize)> {
    let first = *chars.get(i + 1)?;
    if !(first.is_ascii_alphabetic() || first == '/' || first == '!') {
        return None;
    }
    let close = (i + 1..chars.len()).find(|&j| chars[j] == '>')?;
    let inner: String = chars[i + 1..close].iter().collect();
    let end = close + 1;
    if inner.contains("://") && !inner.contains(char::is_whitespace) {
        return Some((Tag::Link(inner), end));
    }
    if inner.contains('@') && !inner.contains(char::is_whitespace) && !inner.contains('/') {
        return Some((Tag::Link(inner), end));
    }
    let name: String = inner
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '/' || *c == '!')
        .collect::<String>()
        .to_ascii_lowercase();
    if name == "img" {
        return Some((
            Tag::Image(attribute(&inner, "alt").unwrap_or_default()),
            end,
        ));
    }
    Some((Tag::Other, end))
}

/// Значение атрибута тега: `alt="Janus"`.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let at = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let quote = rest.chars().next().filter(|c| matches!(c, '"' | '\''))?;
    let value = &rest[1..];
    Some(value[..value.find(quote)?].to_string())
}

/// Сущность HTML: `&amp;`, `&nbsp;`, `&#169;`.
fn entity(chars: &[char], i: usize) -> Option<(char, usize)> {
    let semi = (i + 1..chars.len().min(i + 10)).find(|&j| chars[j] == ';')?;
    let name: String = chars[i + 1..semi].iter().collect();
    let ch = match name.as_str() {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "copy" => '©',
        _ => {
            let code = name.strip_prefix('#')?;
            let n = match code.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => code.parse().ok()?,
            };
            char::from_u32(n)?
        }
    };
    Some((ch, semi + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Doc {
        let lines: Vec<&str> = text.lines().collect();
        render(&lines)
    }

    fn plain(text: &str) -> Vec<String> {
        doc(text).lines.iter().map(Line::plain).collect()
    }

    fn segs(text: &str) -> Vec<Seg> {
        let mut out = Vec::new();
        inline(text, Kind::Plain, Flags::default(), &mut out);
        out
    }

    #[test]
    fn only_markdown_extensions_apply() {
        assert!(applies(Path::new("README.md")));
        assert!(applies(Path::new("notes.Markdown")));
        assert!(!applies(Path::new("index.rst")));
        assert!(!applies(Path::new("main.rs")));
    }

    #[test]
    fn headings_lose_hashes_and_get_rules() {
        let d = doc("# Title\ntext\n## Sub ##\n");
        let p: Vec<String> = d.lines.iter().map(Line::plain).collect();
        assert_eq!(p, ["Title", "", "text", "", "Sub", ""]);
        assert_eq!(d.lines[0].segs[0].kind, Kind::Heading(1));
        assert_eq!(d.lines[1].rule, Some('═'));
        assert_eq!(d.lines[5].rule, Some('─'));
        assert_eq!(d.lines[4].source, 2);
    }

    #[test]
    fn setext_and_html_headings() {
        let d = doc("Title\n=====\n\n<h1 align=\"center\">Janus</h1>\n");
        assert_eq!(d.lines[0].plain(), "Title");
        assert_eq!(d.lines[0].segs[0].kind, Kind::Heading(1));
        assert_eq!(d.lines[3].plain(), "Janus");
    }

    #[test]
    fn paragraph_lines_are_joined() {
        assert_eq!(plain("one\ntwo\n\nthree"), ["one two", "", "three"]);
        assert_eq!(plain("hard  \nbreak"), ["hard", "break"]);
    }

    #[test]
    fn lists_get_bullets_and_nesting() {
        let d = doc("- a\n  continued\n  - b\n1. c\n- [x] done\n");
        let p: Vec<String> = d.lines.iter().map(Line::plain).collect();
        assert_eq!(p, ["• a continued", "  ◦ b", "1. c", "☑ done"]);
        assert_eq!(segs_width(&d.lines[0].cont), 2);
        assert_eq!(segs_width(&d.lines[1].cont), 4);
    }

    #[test]
    fn code_block_is_highlighted_and_kept_verbatim() {
        let d = doc("```rust\nfn main() { *x* }\n```\nafter");
        assert_eq!(d.lines[0].plain(), "  fn main() { *x* }");
        assert!(
            d.lines[0]
                .segs
                .iter()
                .any(|s| s.kind == Kind::Syntax(Token::Keyword))
        );
        assert_eq!(d.lines[1].plain(), "after");
        assert_eq!(d.lines[1].source, 3);
    }

    #[test]
    fn quotes_get_bars() {
        let d = doc("> quoted\n> text\n");
        assert_eq!(d.lines[0].plain(), "▎ quoted text");
        assert_eq!(d.lines[0].segs.last().unwrap().kind, Kind::Quote);
        assert_eq!(d.lines[0].cont[0].text, QUOTE_BAR);
    }

    #[test]
    fn tables_are_aligned() {
        let d =
            doc("| Key | Action |\n|----:|:------|\n| q | quit |\n| Tab | switch \\| panel |\n");
        let p: Vec<String> = d.lines.iter().map(Line::plain).collect();
        assert_eq!(p, ["Key │ Action", "", "q │ quit", "Tab │ switch | panel"]);
        let table = &d.lines[0].row.as_ref().unwrap().table;
        assert_eq!(table.widths, [3, 14]);
        assert_eq!(table.aligns, [Align::Right, Align::Left]);
        assert!(d.lines[0].row.as_ref().unwrap().cells[0][0].strong);
        assert!(d.lines[1].row.as_ref().unwrap().cells.is_empty());
        assert_eq!(d.lines[1].source, 1);
    }

    #[test]
    fn inline_markup() {
        let s = segs("a **b** *c* `d*e` ~~f~~ [g](http://x) snake_case 2 * 3");
        let text: String = s.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(text, "a b c d*e f g snake_case 2 * 3");
        assert!(s.iter().any(|s| s.text == "b" && s.strong));
        assert!(s.iter().any(|s| s.text == "c" && s.emph));
        assert!(s.iter().any(|s| s.text == "d*e" && s.kind == Kind::Code));
        assert!(s.iter().any(|s| s.text == "f" && s.strike));
        assert!(s.iter().any(|s| s.text == "g" && s.kind == Kind::Link));
    }

    #[test]
    fn html_is_stripped() {
        assert_eq!(
            plain(
                "<p align=\"center\">\n  <img src=\"x.svg\" alt=\"Janus\">\n</p>\n\nA &amp; B<br>"
            ),
            ["[Janus]", "", "A & B"]
        );
    }

    #[test]
    fn rules_and_source_mapping() {
        let d = doc("a\n\n---\n\nb");
        assert_eq!(d.lines[2].rule, Some('─'));
        assert_eq!(d.line_of(4), 4);
        assert_eq!(d.to_source(4), 4);
        assert_eq!(d.line_of(100), d.lines.len() - 1);
    }
}
