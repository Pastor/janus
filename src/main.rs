//! Janus — двухпанельный TUI-файловый менеджер.

mod archive;
mod config;
mod drives;
mod editor;
mod git;
mod highlight;
mod i18n;
mod indent;
mod listing;
mod markdown;
mod ops;
mod panel;
mod preview;
mod scan;
mod search;
mod shell;
mod ui;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

use crate::config::Config;
use crate::i18n::Lang;
use crate::ui::theme::ThemeName;

/// Аргументы командной строки. Перекрывают значения из файла конфигурации.
#[derive(Debug, Parser)]
#[command(name = "janus", version, about = "Two-panel terminal file manager")]
struct Args {
    /// Directories for the left and right panels (default: current directory).
    #[arg(num_args = 0..=2)]
    paths: Vec<PathBuf>,

    /// Color theme: dark | light | gruvbox | solarized | nord.
    #[arg(short, long)]
    theme: Option<ThemeName>,

    /// Interface language: en | ru.
    #[arg(short, long)]
    lang: Option<Lang>,

    /// On exit, write the active panel directory to this file.
    #[arg(long, value_name = "FILE")]
    cwd_file: Option<PathBuf>,

    /// Print the shell function `f` that changes into the last directory on exit.
    #[arg(long, value_name = "SHELL")]
    shell_init: Option<shell::Shell>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(shell) = args.shell_init {
        print!("{}", shell::init(shell));
        return Ok(());
    }

    // Испорченный конфиг не повод отказываться работать: стартуем на умолчаниях,
    // но при выходе его не перезаписываем, чтобы пользователь мог его починить.
    let (mut cfg, cfg_ok) = match Config::load() {
        Ok(cfg) => (cfg, true),
        Err(err) => {
            eprintln!("warning: {err:#}");
            (Config::default(), false)
        }
    };

    // Ключи командной строки действуют только на этот запуск: в файл при
    // выходе вернутся прежние тема и язык.
    let stored_look = (cfg.theme, cfg.lang);
    if let Some(theme) = args.theme {
        cfg.theme = theme;
    }
    if let Some(lang) = args.lang {
        cfg.lang = lang;
    }

    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    let left = match args.paths.first() {
        Some(path) => checked_dir(path)?,
        None => cwd.clone(),
    };
    // Правая панель без явного аргумента возвращается туда, где была в прошлый
    // раз: левая следует за местом запуска, правая хранит «второе» рабочее место.
    let right = match args.paths.get(1) {
        Some(path) => checked_dir(path)?,
        None => cfg
            .session
            .right
            .clone()
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| left.clone()),
    };

    let last = ui::run(left, right, cfg, cfg_ok, stored_look)?;
    // Оболочка читает файл после выхода и переходит в эту директорию: сама
    // программа сменить каталог родительского процесса не может.
    if let Some(file) = args.cwd_file {
        std::fs::write(&file, last.as_os_str().as_encoded_bytes())
            .with_context(|| format!("cannot write {}", file.display()))?;
    }
    Ok(())
}

/// Проверяет, что путь существует и ведёт в директорию, и делает его абсолютным.
fn checked_dir(path: &Path) -> Result<PathBuf> {
    let abs =
        std::path::absolute(path).with_context(|| format!("invalid path: {}", path.display()))?;
    if !abs.is_dir() {
        anyhow::bail!("{} is not a directory", path.display());
    }
    Ok(abs)
}
