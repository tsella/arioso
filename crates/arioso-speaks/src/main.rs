//! arioso-speaks: a terminal front end for arioso.
//!
//! One window, one voice, one line of text. The phonemes are shown before they are spoken
//! and can be edited, and the audio can be kept as WAV or FLAC.

mod app;
mod audio;
mod catalog;
mod dither;
mod log;
mod resample;
mod save;
mod ui;

use std::io::stdout;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use arioso_config::Config;
use arioso_models::Manifest;
use crossterm::event::{self, Event, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::App;

/// How long to wait for a key before redrawing the waveform.
const TICK: Duration = Duration::from_millis(60);

fn main() -> Result<()> {
    let config_path = config_path();
    // The config is read twice: once to learn which environment it asks for, and once with
    // those variables in place. Both reads are cheap, and the second one sees the file's own
    // settings where nothing was exported.
    let first = load_config(&config_path)?;
    let log_file = log::init(&first.log.level);
    apply_env(&first.env);
    let config = load_config(&config_path)?;
    let manifest = load_manifest(&config)?;
    let mut app = App::new(config, config_path, manifest).map_err(anyhow::Error::msg)?;
    app.log_file = log_file;

    enable_raw_mode().context("putting the terminal into raw mode")?;
    let mut out = stdout();
    crossterm::execute!(out, EnterAlternateScreen).context("opening the alternate screen")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let result = run(&mut terminal, &mut app);

    disable_raw_mode().ok();
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();
    result
}

/// The loop: draw, wait for a key or a tick, and let the app do what is due.
fn run(terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        app.tick();
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if matches!(ui::handle_key(app, key), ui::Outcome::Quit) {
                        return Ok(());
                    }
                }
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
        if app.quit {
            return Ok(());
        }
    }
}

/// Set the variables the configuration names, without touching one already exported.
///
/// The person at the keyboard wins: a config file is a default, and a variable exported in
/// front of the program has to mean what it says.
fn apply_env(env: &std::collections::BTreeMap<String, String>) {
    for (key, value) in env {
        if std::env::var_os(key).is_some() {
            continue;
        }
        // Safety: this runs before any thread is started, which is the condition the
        // function's own documentation asks for.
        unsafe { std::env::set_var(key, value) };
    }
}

/// Where the config lives: `--config`, then `ARIOSO_CONFIG`, then the data directory.
fn config_path() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        if let Some(value) = argument.strip_prefix("--config=") {
            return PathBuf::from(value);
        }
        if argument == "--config"
            && let Some(value) = args.next()
        {
            return PathBuf::from(value);
        }
    }
    if let Some(explicit) = std::env::var_os("ARIOSO_CONFIG") {
        return PathBuf::from(explicit);
    }
    arioso_models::data_dir().join("config.json")
}

/// The config on disk, or the defaults when there is no file yet.
///
/// A missing file is the first run, not a failure: the app has a catalog and a default voice
/// and can speak without one.
fn load_config(path: &std::path::Path) -> Result<Config> {
    if !path.exists() {
        return Ok(Config::default());
    }
    Config::load_with_env(path).with_context(|| format!("reading {}", path.display()))
}

/// The catalog beside the models.
fn load_manifest(config: &Config) -> Result<Manifest> {
    let dir = arioso_models::cache::models_dir(config);
    let path = dir.join("manifest.json");
    let checked = Manifest::load_checked(
        &path,
        &arioso_models::manifest::signature_path(&path),
        arioso_models::manifest::RELEASE_PUBKEY_HEX,
    )
    .with_context(|| {
        format!(
            "loading the catalog at {} (run `arioso models` to fetch one)",
            path.display()
        )
    })?;
    Ok(checked.manifest)
}
