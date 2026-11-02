//! The `arioso` binary.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use arioso_config::Config;
use arioso_models::{Downloader, Manifest, ModelSpec, Progress, models_dir};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "arioso",
    version,
    about = "Local text to speech and speech to text"
)]
struct Cli {
    /// Config file. Defaults to the platform config directory.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect and fetch models.
    Models {
        #[command(subcommand)]
        command: ModelsCommand,
    },
    /// Check the install: config, cache directory, manifest.
    Doctor,
    /// Print or export configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
enum ModelsCommand {
    /// List catalog models and whether they are installed.
    List,
    /// Download one model, resuming where a previous run stopped.
    Download {
        /// Model id from the manifest.
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the JSON Schema for the config file.
    Schema,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(default_config_path);
    let config = Config::load_with_env(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    init_tracing(&config);

    match cli.command {
        Command::Models { command } => match command {
            ModelsCommand::List => list_models(&config),
            ModelsCommand::Download { id } => download(&config, &id),
        },
        Command::Doctor => doctor(&config, &config_path),
        Command::Config { command } => match command {
            ConfigCommand::Schema => {
                println!("{}", serde_json::to_string_pretty(&Config::json_schema())?);
                Ok(())
            }
        },
    }
}

fn default_config_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("ARIOSO_CONFIG") {
        return PathBuf::from(explicit);
    }
    arioso_models::data_dir().join("config.json")
}

fn manifest_path(config: &Config) -> PathBuf {
    models_dir(config).join("manifest.json")
}

fn load_manifest(config: &Config) -> Result<Manifest> {
    let path = manifest_path(config);
    Manifest::load(&path).with_context(|| {
        format!(
            "loading the model manifest at {} (place a manifest.json there)",
            path.display()
        )
    })
}

fn list_models(config: &Config) -> Result<()> {
    let manifest = load_manifest(config)?;
    let dir = models_dir(config);
    println!(
        "{:<34} {:<5} {:>10}  {:<10} state",
        "id", "kind", "size", "license"
    );
    for model in &manifest.models {
        let installed = model_installed(model, &dir);
        let state = if installed { "installed" } else { "missing" };
        println!(
            "{:<34} {:<5} {:>10}  {:<10} {}",
            model.id,
            format!("{:?}", model.kind).to_lowercase(),
            human_bytes(model.total_size()),
            model.license,
            state
        );
    }
    Ok(())
}

fn download(config: &Config, id: &str) -> Result<()> {
    let manifest = load_manifest(config)?;
    let spec = manifest.find(id).with_context(|| {
        format!(
            "no model with id {id} in {}",
            manifest_path(config).display()
        )
    })?;
    let dir = models_dir(config);
    let total = spec.total_size();
    println!(
        "fetching {} ({} bytes) into {}",
        spec.id,
        total,
        dir.display()
    );

    let runtime = tokio::runtime::Runtime::new()?;
    let mut last_reported = u64::MAX;
    let mut progress = |p: Progress| {
        let step = total / 20;
        if p.downloaded == p.total
            || last_reported == u64::MAX
            || p.downloaded.saturating_sub(last_reported) >= step
        {
            last_reported = p.downloaded;
            eprint!(
                "\r  {:>6.1}%  {} / {}",
                p.percent(),
                human_bytes(p.downloaded),
                human_bytes(p.total)
            );
        }
    };
    let paths = runtime.block_on(async {
        Downloader::new()?
            .ensure_model(spec, &dir, &mut progress)
            .await
    })?;
    eprintln!();
    println!("ready: {} file(s)", paths.len());
    Ok(())
}

fn doctor(config: &Config, config_path: &Path) -> Result<()> {
    let mut problems = 0usize;
    println!("config      {}", config_path.display());
    match config.validate() {
        Ok(()) => println!("  valid"),
        Err(e) => {
            problems += 1;
            println!("  INVALID: {e}");
        }
    }

    let dir = models_dir(config);
    match std::fs::create_dir_all(&dir) {
        Ok(()) => println!("models dir  {} (writable)", dir.display()),
        Err(e) => {
            problems += 1;
            println!("models dir  {} UNWRITABLE: {e}", dir.display());
        }
    }

    let manifest = manifest_path(config);
    if manifest.exists() {
        match Manifest::load(&manifest) {
            Ok(m) => {
                let installed = m
                    .models
                    .iter()
                    .filter(|spec| model_installed(spec, &dir))
                    .count();
                println!(
                    "manifest    {} models, {installed} installed",
                    m.models.len()
                );
            }
            Err(e) => {
                problems += 1;
                println!("manifest    INVALID: {e}");
            }
        }
    } else {
        println!("manifest    absent at {}", manifest.display());
        println!("            copy models/manifest.json there, then run `arioso models list`");
    }

    if problems > 0 {
        bail!("{problems} problem(s) found");
    }
    println!("ok");
    Ok(())
}

fn init_tracing(config: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.log.level.clone()));
    if config.log.format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

/// A model counts as installed only when every file is present at the right size.
fn model_installed(model: &ModelSpec, dir: &Path) -> bool {
    model.files.iter().all(|file| {
        std::fs::metadata(dir.join(&file.name))
            .map(|meta| meta.len() == file.size)
            .unwrap_or(false)
    })
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
