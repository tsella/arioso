//! The `arioso` binary.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use arioso_config::Config;
use arioso_g2p::CmuDict;
use arioso_models::{Downloader, Manifest, ModelSpec, Progress, models_dir};
use arioso_tts_piper::{PiperVoice, wav::write_wav};
use clap::{Parser, Subcommand};
use std::sync::Arc;
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
    /// Serve the HTTP API.
    Serve,
    /// Show how text is expanded before it reaches the voice.
    Normalize {
        /// Text to expand.
        text: String,
    },
    /// Synthesize text to a WAV file.
    Say {
        /// Text to speak.
        text: String,
        /// Output file. Defaults to out.wav.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Pronunciation dictionary. Defaults to cmudict.dict in the models directory.
        #[arg(long)]
        dict: Option<PathBuf>,
    },
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
        Command::Say { text, out, dict } => say(&config, &text, out, dict),
        Command::Serve => serve(config),
        Command::Normalize { text } => {
            println!("{}", arioso_core::normalize(&text));
            Ok(())
        }
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

fn serve(config: Config) -> Result<()> {
    let manifest = load_manifest(&config)?;
    let state = Arc::new(arioso_server::AppState::new(config.clone(), manifest)?);
    let bind = config.server.bind.clone();

    tokio::runtime::Runtime::new()?.block_on(async move {
        let listener = tokio::net::TcpListener::bind(&bind)
            .await
            .with_context(|| format!("binding {bind}"))?;
        println!("listening on http://{bind}");
        axum::serve(listener, arioso_server::router(state)).await?;
        Ok::<(), anyhow::Error>(())
    })
}

fn say(config: &Config, text: &str, out: Option<PathBuf>, dict: Option<PathBuf>) -> Result<()> {
    let manifest = load_manifest(config)?;
    let dir = models_dir(config);
    let voices: Vec<&ModelSpec> = manifest
        .models
        .iter()
        .filter(|m| m.kind == arioso_models::Kind::Tts && m.engine == "piper")
        .collect();
    let model = voices
        .iter()
        .find(|m| m.id == config.tts.voice || m.id.ends_with(&config.tts.voice))
        .copied()
        .with_context(|| {
            let available = voices
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "no piper voice matches tts.voice {:?}; available: {available}",
                config.tts.voice
            )
        })?;
    let file = |suffix: &str| {
        model
            .files
            .iter()
            .find(|f| f.name.ends_with(suffix))
            .map(|f| dir.join(&f.name))
            .with_context(|| format!("voice {} has no {suffix} file", model.id))
    };
    let onnx = file(".onnx")?;
    let voice_config = file(".onnx.json")?;

    let dict_path = dict.unwrap_or_else(|| dir.join("cmudict.dict"));
    let dict_text = std::fs::read_to_string(&dict_path)
        .with_context(|| format!("reading {}", dict_path.display()))?;
    let dictionary = CmuDict::parse(&dict_text);

    let mut voice = PiperVoice::load(&onnx, &voice_config)?;
    let mut samples: Vec<f32> = Vec::new();
    let missing = voice.render(text, &dictionary, config.tts.speed, |audio, _| {
        samples.extend_from_slice(audio);
        Ok(())
    })?;

    if samples.is_empty() {
        bail!("nothing to say: none of the words are in the dictionary");
    }

    let out = out.unwrap_or_else(|| PathBuf::from("out.wav"));
    let rate = voice.sample_rate();
    write_wav(&out, &samples, rate)?;
    println!(
        "wrote {} ({:.2} s at {rate} Hz)",
        out.display(),
        samples.len() as f32 / rate as f32
    );
    if !missing.is_empty() {
        let shown = missing
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = if missing.len() > 5 { ", ..." } else { "" };
        eprintln!(
            "{} word(s) not in the dictionary and skipped: {shown}{more}",
            missing.len()
        );
    }
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
