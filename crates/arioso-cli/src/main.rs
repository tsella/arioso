//! The `arioso` binary.

use std::io::Write as _;
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
    /// Stop a sidecar started with `server.sidecar` on.
    Stop,
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
    /// Transcribe a WAV file to text.
    Transcribe {
        /// Audio file. Mono, 16-bit PCM or 32-bit float, any rate.
        file: PathBuf,
        /// Print JSON instead of plain text.
        #[arg(long)]
        json: bool,
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
        Command::Say { text, out, dict } => say(&config, &config_path, &text, out, dict),
        Command::Transcribe { file, json } => transcribe(&config, &config_path, &file, json),
        Command::Serve => serve(config, &config_path),
        Command::Stop => stop(&config_path),
        Command::Normalize { text } => {
            println!(
                "{}",
                arioso_core::normalize_with(
                    &text,
                    arioso_core::NormalizeOptions {
                        time_of_day: config.tts.time_of_day,
                    },
                )
            );
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
    // "checking" rather than "fetching": a model that is already present is
    // hashed and skipped, and the old wording read as if it downloaded again.
    println!(
        "checking {} ({} bytes) in {}",
        spec.id,
        total,
        dir.display()
    );

    let mirror = config.models.mirror.clone();
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
        Downloader::with_mirror(mirror)?
            .ensure_model(spec, &dir, &mut progress)
            .await
    })?;
    eprintln!();
    println!("ready: {} file(s)", paths.len());
    Ok(())
}

/// The marker that tells a child process it is the server, not the launcher.
const SIDECAR_CHILD: &str = "ARIOSO_SIDECAR_CHILD";

/// Where the child reports the port it bound, so the launcher never guesses.
const SIDECAR_READY: &str = "ARIOSO_SIDECAR_READY";

fn serve(config: Config, config_path: &Path) -> Result<()> {
    if config.server.sidecar && std::env::var_os(SIDECAR_CHILD).is_none() {
        return detach(config_path);
    }
    serve_here(config, config_path)
}

/// Serve in this process, reporting the port before serving it.
fn serve_here(config: Config, config_path: &Path) -> Result<()> {
    let manifest = load_manifest(&config)?;
    let sidecar = config.server.sidecar;
    let state = Arc::new(arioso_server::AppState::new(
        config.clone(),
        manifest,
        Some(config_path),
    )?);
    // A config that names models to fetch at start does not wait for a caller to ask.
    state.spawn_auto_downloads();
    let bind = config.server.bind.clone();
    let tls = config.server.tls.clone();
    let header_timeout = config.server.limits.header_timeout_ms;
    let config_grace = config.server.limits.shutdown_grace_ms;

    // Bound here rather than inside the server, because the port has to be known before
    // anything is printed: with port 0 the operating system chooses it, and a caller
    // cannot guess what it chose.
    let listener = bind_listener(&bind, sidecar)?;
    let local = listener
        .local_addr()
        .context("the bound address could not be read")?;
    // Known only after the bind, so the file is named for the port it ended up with.
    let pid_path = pid_file(config_path, local.port());

    // Announced only after the server object exists: a socket that is bound is not a
    // server that can answer, and a caller told the port a moment too early connects to
    // nothing. The closure is called from both branches below.
    let scheme = if tls.is_configured() { "https" } else { "http" };
    let announce_pid = pid_path.clone();
    let announce = move || -> Result<Option<std::fs::File>> {
        if sidecar {
            // The lock comes first: a child that cannot claim its PID file must fail before
            // it prints a port, not after, or the caller reads a port that never serves.
            let lock = hold_pid_file(&announce_pid)?;
            // The one line a caller reads, flushed before anything else can fail so a pipe
            // never waits on a buffer.
            println!("START {}", local.port());
            std::io::stdout().flush()?;
            if let Some(ready) = std::env::var_os(SIDECAR_READY) {
                let _ = std::fs::write(PathBuf::from(ready), format!("port {}\n", local.port()));
            }
            return Ok(Some(lock));
        } else {
            println!("listening on {scheme}://{local}");
        }
        Ok(None)
    };

    tokio::runtime::Runtime::new()?.block_on(async move {
        // The accept loop's handle, so a route can stop the server and so a signal is a
        // request to stop rather than a reason to drop what is in flight.
        let handle = axum_server::Handle::<std::net::SocketAddr>::new();
        state.attach_handle(handle.clone());
        {
            let handle = handle.clone();
            tokio::spawn(async move {
                wait_for_stop_signal().await;
                tracing::info!(grace_ms = config_grace, "stopping on a signal");
                handle.graceful_shutdown(Some(std::time::Duration::from_millis(config_grace)));
            });
        }
        let router = arioso_server::router(state);
        let service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
        let timeout = std::time::Duration::from_millis(header_timeout);
        // Two branches rather than one: a TLS acceptor changes the server's type, so the
        // two configurations cannot share a binding. The timeout setup is repeated
        // deliberately; a macro to save four lines would hide what it configures.
        let outcome = if tls.is_configured() {
            // Validation already refused one half of the pair, so both are here.
            let (cert, key) = match (tls.cert.clone(), tls.key.clone()) {
                (Some(cert), Some(key)) => (cert, key),
                _ => anyhow::bail!("server.tls needs both cert and key"),
            };
            let tls_config = arioso_server::tls_config(&cert, &key).await?;
            // Held for the life of the server: dropping it stops the certificate watch.
            let _reload = arioso_server::watch_certificates(tls_config.clone(), cert, key);
            let mut server =
                axum_server::from_tcp_rustls(listener, tls_config)?.handle(handle.clone());
            let _pid_lock = announce()?;
            // A client that opens a socket and never finishes its headers holds a task for
            // as long as it likes. The timeout is the difference between a slow client and
            // a cheap denial of service.
            server
                .http_builder()
                .http1()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(timeout);
            server.serve(service).await
        } else {
            let mut server = axum_server::from_tcp(listener)?.handle(handle.clone());
            let _pid_lock = announce()?;
            server
                .http_builder()
                .http1()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(timeout);
            server.serve(service).await
        };
        if sidecar {
            // A process that stopped on request has no PID to point at. Left behind, the
            // file makes the next `stop` look at a number the system has reused.
            let _ = std::fs::remove_file(&pid_path);
        }
        outcome?;
        Ok::<(), anyhow::Error>(())
    })
}

/// Resolve when the process is asked to stop.
///
/// SIGTERM and Ctrl-C both mean "finish what you are doing and go", which is what a
/// service manager sends. The handle turns that into a graceful shutdown; a kill that
/// cannot wait is what `abort` is for.
async fn wait_for_stop_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not listen for SIGTERM; only Ctrl-C will stop this process");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Start the server in a child process and let the caller read its port from stdout.
///
/// No `fork` and no `setsid`: the same code runs on macOS, Linux and Windows. The child
/// inherits our stdout and stderr on purpose, so a caller reads `START {port}` from the
/// pipe it gave us and a child that fails says why in the same place.
fn detach(config_path: &Path) -> Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    let ready = std::env::temp_dir().join(format!(
        "arioso-sidecar-{}-{stamp}.ready",
        std::process::id()
    ));
    let exe = std::env::current_exe().context("finding the running binary")?;
    let mut child = std::process::Command::new(exe)
        .arg("--config")
        .arg(config_path)
        .arg("serve")
        .env(SIDECAR_CHILD, "1")
        .env(SIDECAR_READY, &ready)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting a sidecar for {}", config_path.display()))?;

    // A caller must never connect to a port that is not listening yet, and a child that
    // dies must not leave the caller waiting for a line that will not come.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(report) = std::fs::read_to_string(&ready) {
            let _ = std::fs::remove_file(&ready);
            let report = report.trim().to_owned();
            return match report.strip_prefix("port ") {
                Some(_) => Ok(()),
                None => anyhow::bail!("the sidecar reported: {report}"),
            };
        }
        if let Some(status) = child.try_wait()? {
            let _ = std::fs::remove_file(&ready);
            anyhow::bail!("the sidecar exited before it was ready ({status})");
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = std::fs::remove_file(&ready);
            anyhow::bail!("the sidecar did not report a port within 30 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Bind the configured address, falling back to a port the system chooses.
///
/// A sidecar reports the port it got, so a taken port is not a failure: it is a reason to
/// take another. Anywhere else a taken port is an error the operator must see.
fn bind_listener(bind: &str, may_choose_port: bool) -> Result<std::net::TcpListener> {
    // The same resolution the config validated, so the socket takes the address the
    // rules were applied to.
    let addr = arioso_config::bind_address(bind)
        .with_context(|| format!("server.bind {bind:?} is not an address"))?;
    // Tokio refuses to register a blocking socket, and this one is handed straight to it.
    let prepare = |listener: std::net::TcpListener| -> Result<std::net::TcpListener> {
        listener
            .set_nonblocking(true)
            .context("making the listening socket non-blocking")?;
        Ok(listener)
    };
    match std::net::TcpListener::bind(addr) {
        Ok(listener) => prepare(listener),
        Err(error) if may_choose_port && error.kind() == std::io::ErrorKind::AddrInUse => {
            let fallback = std::net::SocketAddr::new(addr.ip(), 0);
            let listener = std::net::TcpListener::bind(fallback)
                .with_context(|| format!("binding {fallback} after {addr} was taken"))?;
            prepare(listener)
        }
        Err(error) => Err(error).with_context(|| format!("binding {addr}")),
    }
}

/// The name every PID file starts with.
///
/// Named after the program, not the config file, so a PID file is recognisable in a
/// directory listing without knowing which config wrote it.
const PID_PREFIX: &str = "arioso-";

/// The PID file for one sidecar, keyed by port.
///
/// Keyed by port because one config can run more than one sidecar: the second takes the
/// port the first did not want. A single file per config would have them overwrite each
/// other, and `stop` would then kill whichever wrote last.
fn pid_file(config_path: &Path, port: u16) -> PathBuf {
    config_path.with_file_name(format!("{PID_PREFIX}{port}.pid"))
}

/// Write the PID and hold an exclusive lock on the file for as long as this process runs.
///
/// The lock is what makes the number trustworthy. Without it a file left by a crash names
/// a process that no longer exists, and the system will hand that number to something else;
/// `stop` would then signal a stranger. A file that can be locked has no owner, which is
/// how a stale entry is told from a live one.
#[cfg(unix)]
fn hold_pid_file(path: &Path) -> Result<std::fs::File> {
    use std::io::Write as _;
    use std::os::unix::io::AsRawFd as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        // Not truncated here on purpose: the file is emptied after the lock, so a process
        // that loses the race cannot wipe the winner's contents.
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!(
            "{} is locked by another sidecar for this port",
            path.display()
        );
    }
    // Truncated only once the lock is held. Truncating at open would let a second process
    // empty the winner's file while it runs, and `stop` would then read nothing.
    file.set_len(0)
        .with_context(|| format!("emptying {}", path.display()))?;
    writeln!(file, "{}", std::process::id())
        .with_context(|| format!("writing {}", path.display()))?;
    // The lock lives as long as this handle, which the caller keeps until the server ends.
    Ok(file)
}

#[cfg(not(unix))]
fn hold_pid_file(path: &Path) -> Result<std::fs::File> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(file, "{}", std::process::id())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(file)
}

/// The process that owns a PID file, or `None` when the file is left over from one that ended.
#[cfg(unix)]
fn pid_file_owner(path: &Path) -> Option<u32> {
    use std::os::unix::io::AsRawFd as _;
    let file = std::fs::OpenOptions::new().write(true).open(path).ok()?;
    // Taking the lock means nobody holds it, so the number in the file belongs to a process
    // that has ended. Releasing it immediately keeps `stop` from blocking a later start.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return None;
    }
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

#[cfg(not(unix))]
fn pid_file_owner(path: &Path) -> Option<u32> {
    // Windows has no advisory lock here, so the name is checked instead: a PID that now
    // belongs to another program is not this sidecar.
    let pid: u32 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    if text.contains("arioso") {
        Some(pid)
    } else {
        None
    }
}

/// Every PID file beside this config, with the port read back out of the name.
fn pid_files(config_path: &Path) -> Vec<(PathBuf, u16)> {
    let prefix = PID_PREFIX;
    let directory = config_path.parent().unwrap_or_else(|| Path::new("."));
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(port) = name
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(".pid"))
            .and_then(|port| port.parse::<u16>().ok())
        else {
            continue;
        };
        found.push((entry.path(), port));
    }
    found.sort_by_key(|(_, port)| *port);
    found
}

/// Stop every sidecar this config started.
fn stop(config_path: &Path) -> Result<()> {
    let files = pid_files(config_path);
    if files.is_empty() {
        anyhow::bail!(
            "no sidecar is running for {}: no {PID_PREFIX}*.pid file beside it",
            config_path.display()
        );
    }
    let mut stopped = 0;
    for (path, port) in files {
        let Some(pid) = pid_file_owner(&path) else {
            println!(
                "port {port}: the process that wrote {} has ended",
                path.display()
            );
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if running(pid) {
            request_stop(pid)?;
            // A process that has been asked to stop has not stopped yet. Removing the file
            // before it is gone would leave a running server nobody can find.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while running(pid) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if running(pid) {
                anyhow::bail!("process {pid} on port {port} did not stop within 10 seconds");
            }
            stopped += 1;
            println!("stopped {pid} on port {port}");
        } else {
            println!("process {pid} on port {port} was already gone");
        }
        let _ = std::fs::remove_file(&path);
    }
    println!("{stopped} sidecar(s) stopped");
    Ok(())
}

#[cfg(unix)]
fn running(pid: u32) -> bool {
    // Signal 0 asks the kernel whether the process exists without touching it.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(unix)]
fn request_stop(pid: u32) -> Result<()> {
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if result != 0 {
        anyhow::bail!(
            "could not ask process {pid} to stop: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

#[cfg(windows)]
fn running(pid: u32) -> bool {
    // tasklist exits 0 whether or not the pid is there, so the answer is in the output.
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}

#[cfg(windows)]
fn request_stop(pid: u32) -> Result<()> {
    let status = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .status()
        .context("running taskkill")?;
    if !status.success() {
        anyhow::bail!("taskkill could not stop process {pid}");
    }
    Ok(())
}

fn say(
    config: &Config,
    config_path: &Path,
    text: &str,
    out: Option<PathBuf>,
    dict: Option<PathBuf>,
) -> Result<()> {
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

    let user_path = config.dictionary_path(Some(config_path));
    let user = match arioso_core::UserDictionary::load(&user_path) {
        Ok(dictionary) => dictionary,
        Err(arioso_core::DictError::Io { .. }) => arioso_core::UserDictionary::shipped(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", user_path.display()));
        }
    };

    let mut voice = PiperVoice::load(&onnx, &voice_config)?;
    let mut samples: Vec<f32> = Vec::new();
    let options = arioso_tts_piper::RenderOptions {
        speed: config.tts.speed,
        normalize: arioso_core::NormalizeOptions {
            time_of_day: config.tts.time_of_day,
        },
    };
    let report = voice.render(text, &dictionary, &user, options, |audio, _| {
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
    if !report.missing.is_empty() {
        let shown = report
            .missing
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = if report.missing.len() > 5 {
            ", ..."
        } else {
            ""
        };
        eprintln!(
            "{} word(s) not in the dictionary and skipped: {shown}{more}",
            report.missing.len()
        );
    }
    if !report.guessed.is_empty() {
        let shown = report
            .guessed
            .iter()
            .take(5)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let more = if report.guessed.len() > 5 {
            ", ..."
        } else {
            ""
        };
        eprintln!(
            "{} word(s) not in the dictionary, spoken by spelling rules: {shown}{more}",
            report.guessed.len()
        );
    }
    if !report.unusable.is_empty() {
        eprintln!(
            "{} entr(y/ies) the voice cannot say: {}",
            report.unusable.len(),
            report.unusable.join(", ")
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

    problems += report_dictionary(config, config_path, &dir)?;

    if problems > 0 {
        bail!("{problems} problem(s) found");
    }
    println!("ok");
    Ok(())
}

/// Report the user dictionary, and the entries the configured voice cannot say.
fn report_dictionary(config: &Config, config_path: &Path, models: &Path) -> Result<usize> {
    let path = config.dictionary_path(Some(config_path));
    let user = match arioso_core::UserDictionary::load(&path) {
        Ok(user) => user,
        Err(arioso_core::DictError::Io { .. }) => {
            println!("dictionary  absent at {} (optional)", path.display());
            return Ok(0);
        }
        Err(error) => {
            println!("dictionary  {} INVALID: {error}", path.display());
            return Ok(1);
        }
    };
    println!("dictionary  {} entries from {}", user.len(), path.display());

    // The voice's symbol inventory lives in its config, so no model is loaded.
    let voice_config = std::fs::read_dir(models)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.to_string_lossy().ends_with(".onnx.json")
                && path.to_string_lossy().contains(&config.tts.voice)
        });
    let Some(voice_config) = voice_config else {
        println!(
            "            voice {} is not installed, so entries were not checked",
            config.tts.voice
        );
        return Ok(0);
    };

    let map = arioso_g2p::PhonemeMap::from_voice_config(&std::fs::read_to_string(&voice_config)?)?;
    let dict_path = models.join("cmudict.dict");
    let pronunciations = match std::fs::read_to_string(&dict_path) {
        Ok(text) => CmuDict::parse(&text),
        Err(_) => {
            println!(
                "            no cmudict.dict in {}, so entries were not checked",
                models.display()
            );
            return Ok(0);
        }
    };

    // Two questions per entry: can the voice say it at all, and can it say it
    // without guessing. Spelling rules can pronounce almost any letter string,
    // so the second question is the one that finds a typo.
    let options = arioso_core::NormalizeOptions::default();
    let mut unusable: Vec<&str> = Vec::new();
    let mut by_rule: Vec<&str> = Vec::new();
    for entry in user.entries() {
        let sayable = |rules: bool| {
            entry.forms().iter().any(|form| {
                arioso_tts_piper::form_symbols(form, &map, &pronunciations, options, rules)
                    .is_some()
            })
        };
        if !sayable(true) {
            unusable.push(&entry.term);
        } else if !sayable(false) {
            by_rule.push(&entry.term);
        }
    }

    if !by_rule.is_empty() {
        println!(
            "            {} entr(y/ies) spoken by spelling rules: {}",
            by_rule.len(),
            by_rule.join(", ")
        );
    }
    if unusable.is_empty() {
        println!(
            "            every entry can be said by {}",
            config.tts.voice
        );
        Ok(0)
    } else {
        println!(
            "            {} entr(y/ies) this voice cannot say: {}",
            unusable.len(),
            unusable.join(", ")
        );
        Ok(1)
    }
}

/// Load the speech to text model the config names.
///
/// The manifest names the three files, so a models directory holding more than one
/// model still loads the right one. Without a manifest entry the loader falls back to
/// searching, which refuses a directory holding files from two models.
fn load_engine(
    dir: &Path,
    model: &str,
    language: Option<&str>,
    threads: usize,
) -> Result<arioso_stt_sherpa::Engine> {
    let manifest_path = dir.join("manifest.json");
    if let Ok(text) = std::fs::read_to_string(&manifest_path) {
        if let Ok(manifest) = arioso_models::Manifest::parse(text.as_bytes()) {
            if let Some(spec) = manifest.models.iter().find(|spec| spec.id == model) {
                let paths: Vec<std::path::PathBuf> =
                    spec.files.iter().map(|f| dir.join(&f.name)).collect();
                let model_type = spec
                    .meta
                    .get("model_type")
                    .and_then(serde_json::Value::as_str);
                return Ok(arioso_stt_sherpa::Engine::from_manifest_files(
                    dir, &paths, model_type, language, threads,
                )?);
            }
        }
    }
    Ok(arioso_stt_sherpa::Engine::load(dir, language, threads)?)
}

/// Transcribe a file, with the user dictionary applied to the result.
fn transcribe(config: &Config, config_path: &Path, file: &Path, json: bool) -> Result<()> {
    if !config.stt.enabled {
        bail!("speech to text is off in the config (stt.enabled)");
    }
    let dir = models_dir(config);
    let (samples, rate) =
        arioso_tts_piper::wav::read(file).with_context(|| format!("reading {}", file.display()))?;

    let language = if config.stt.language == "auto" {
        None
    } else {
        Some(config.stt.language.as_str())
    };
    // Half the cores is the sweet spot for whisper on a laptop; more threads mostly
    // fight each other.
    let threads =
        std::thread::available_parallelism().map_or(4, |count| (count.get() / 2).clamp(1, 8));
    let engine =
        load_engine(&dir, &config.stt.batch_model, language, threads).with_context(|| {
            format!(
                "loading {} from {}. Run `arioso models download {}` if it is missing.",
                config.stt.batch_model,
                dir.display(),
                config.stt.batch_model
            )
        })?;
    let transcript = engine.transcribe(&samples, rate)?;

    // The dictionary runs backwards as well: every spoken form maps to its term, so a
    // transcript comes back spelled the way the user writes it.
    let user_path = config.dictionary_path(Some(config_path));
    let user = match arioso_core::UserDictionary::load(&user_path) {
        Ok(dictionary) => dictionary,
        Err(arioso_core::DictError::Io { .. }) => arioso_core::UserDictionary::shipped(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", user_path.display()));
        }
    };
    // The correction compares sounds, so it needs the same pronunciation source the
    // voice uses.
    let dict_path = dir.join("cmudict.dict");
    let dict_text = std::fs::read_to_string(&dict_path)
        .with_context(|| format!("reading {}", dict_path.display()))?;
    let dictionary = CmuDict::parse(&dict_text);
    let text = arioso_g2p::reverse::correct(&user, &dictionary, &transcript.text);

    if json {
        let seconds = if rate == 0 {
            0.0
        } else {
            samples.len() as f64 / f64::from(rate)
        };
        println!(
            "{}",
            serde_json::json!({ "text": text, "seconds": (seconds * 100.0).round() / 100.0 })
        );
    } else {
        println!("{text}");
    }
    Ok(())
}

fn init_tracing(config: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.log.level.clone()));
    // stderr unless the config names a file. stdout belongs to the caller: in sidecar mode
    // it carries the port line and nothing else, and a log line on that pipe would be read
    // as the port.
    match &config.log.file {
        Some(path) => match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            Ok(file) => {
                let writer = std::sync::Mutex::new(file);
                if config.log.format == "json" {
                    tracing_subscriber::fmt()
                        .with_env_filter(filter)
                        .with_writer(writer)
                        .json()
                        .init();
                } else {
                    tracing_subscriber::fmt()
                        .with_env_filter(filter)
                        .with_writer(writer)
                        .init();
                }
            }
            Err(error) => {
                // A log file that cannot be opened must not stop the process, but it must
                // not be silent either: the operator asked for a file and gets stderr.
                eprintln!(
                    "log.file {} could not be opened ({error}); logging to stderr",
                    path.display()
                );
                init_tracing_to_stderr(config, filter);
            }
        },
        None => init_tracing_to_stderr(config, filter),
    }
}

/// The default sink.
fn init_tracing_to_stderr(config: &Config, filter: EnvFilter) {
    if config.log.format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
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
