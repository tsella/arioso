//! The `arioso` binary.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use arioso_config::Config;
use arioso_g2p::CmuDict;
use arioso_models::{
    Downloader, Manifest, ManifestError, ModelSpec, Progress, RELEASE_PUBKEY_HEX, SignatureState,
    models_dir, signature_path,
};
use arioso_tts_kokoro::KokoroVoice;
use arioso_tts_piper::{PiperVoice, form_symbols};
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
    /// Check the install: config, cache directory, manifest, certificate.
    Doctor {
        /// Treat warnings as failures, such as a certificate with fewer than thirty days left.
        #[arg(long)]
        strict: bool,
    },
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
        /// Speech engine to speak with. Defaults to tts.engine.
        #[arg(long)]
        engine: Option<String>,
        /// Voice to speak with. Defaults to tts.voice, or to the engine's own default when
        /// --engine names the other engine.
        #[arg(long)]
        voice: Option<String>,
        /// Pronunciation dictionary. Defaults to cmudict.dict in the models directory.
        #[arg(long)]
        dict: Option<PathBuf>,
        /// Speaker number on a multi-speaker voice, such as en_GB-semaine-medium.
        #[arg(long)]
        sid: Option<i64>,
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
    List {
        /// Only models for this language code, such as `en_US` or `pt`.
        #[arg(long)]
        language: Option<String>,
        /// Only speech to text, or only text to speech.
        #[arg(long)]
        kind: Option<String>,
        /// Only models whose files are on disk.
        #[arg(long)]
        installed: bool,
    },
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
            ModelsCommand::List {
                language,
                kind,
                installed,
            } => list_models(
                &config,
                ModelFilter {
                    language: language.as_deref(),
                    kind: kind.as_deref(),
                    installed_only: installed,
                },
            ),
            ModelsCommand::Download { id } => download(&config, &id),
        },
        Command::Doctor { strict } => doctor(&config, &config_path, strict),
        Command::Say {
            text,
            out,
            engine,
            voice,
            dict,
            sid,
        } => say(
            &config,
            &config_path,
            SayRequest {
                text: &text,
                out,
                dict,
                sid,
                engine: engine.as_deref(),
                voice: voice.as_deref(),
            },
        ),
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

/// Which models `arioso models list` prints.
struct ModelFilter<'a> {
    /// A language code or its family: `en_US` matches one voice set, `pt` matches every
    /// Portuguese one.
    language: Option<&'a str>,
    /// `tts` or `stt`.
    kind: Option<&'a str>,
    /// Skip anything that is not on disk.
    installed_only: bool,
}

fn list_models(config: &Config, filter: ModelFilter<'_>) -> Result<()> {
    let manifest = load_manifest(config)?;
    let dir = models_dir(config);
    // Written through a locked handle and errors dropped: `arioso models list | head` closes
    // the pipe, and a catalog of 181 rows is exactly what a person pipes.
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "{:<34} {:<5} {:>10}  {:<10} state",
        "id", "kind", "size", "license"
    );
    let mut shown = 0;
    for model in &manifest.models {
        let installed = model_installed(model, &dir);
        let kind = format!("{:?}", model.kind).to_lowercase();
        if filter.installed_only && !installed {
            continue;
        }
        if filter.kind.is_some_and(|wanted| wanted != kind) {
            continue;
        }
        if let Some(wanted) = filter.language {
            let wanted = wanted.replace('_', "-");
            // A catalog with 178 voices is unreadable as one list, so the filter is the
            // difference between a catalog and a wall.
            let languages = model
                .meta
                .get("languages")
                .and_then(serde_json::Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(serde_json::Value::as_str)
                        .any(|code| {
                            let code = code.replace('_', "-");
                            code.eq_ignore_ascii_case(&wanted)
                                || code
                                    .split('-')
                                    .next()
                                    .is_some_and(|family| family.eq_ignore_ascii_case(&wanted))
                        })
                })
                .unwrap_or(false);
            if !languages {
                continue;
            }
        }
        shown += 1;
        let _ = writeln!(
            out,
            "{:<34} {:<5} {:>10}  {:<10} {}",
            model.id,
            kind,
            human_bytes(model.total_size()),
            model.license,
            if installed { "installed" } else { "missing" }
        );
    }
    if shown == 0 {
        let _ = writeln!(out, "no models match");
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

/// What `arioso say` was asked to speak, and where the audio goes.
struct SayRequest<'a> {
    /// Text to speak.
    text: &'a str,
    /// Where the WAV goes. Defaults to out.wav.
    out: Option<PathBuf>,
    /// Pronunciation dictionary, or the models directory's.
    dict: Option<PathBuf>,
    /// Speaker number, for a voice that has more than one.
    sid: Option<i64>,
    /// Engine, when the flag overrides the config.
    engine: Option<&'a str>,
    /// Voice, when the flag overrides the config.
    voice: Option<&'a str>,
}

fn say(config: &Config, config_path: &Path, request: SayRequest<'_>) -> Result<()> {
    // The flags win over the file, so one command can speak in the other engine without
    // editing a config that the server is also reading.
    let engine = request.engine.unwrap_or(&config.tts.engine);
    let (voice, note) = resolve_voice(config, engine, request.voice);
    if let Some(note) = note {
        eprintln!("{note}");
    }
    if engine == "kokoro" && request.sid.unwrap_or(0) != 0 {
        bail!("the kokoro engine has one speaker per voice, so --sid has nothing to choose");
    }
    let (dictionary, user) = dictionaries(config, config_path, request.dict.as_deref())?;
    match engine {
        "kokoro" => say_kokoro(config, &request, &voice, &dictionary, &user),
        "piper" => say_piper(config, &request, &voice, &dictionary, &user),
        other => bail!(
            "tts.engine {other} is not built into this binary; the engines here are piper, kokoro"
        ),
    }
}

/// The voice this `say` will use, and what to tell the caller when it is not the config's.
///
/// A voice belongs to one engine, and the two engines share no names, so speaking with the
/// other engine while the config names this one's voice cannot work. The engine's own
/// default stands in, and the note says which voice that is rather than leaving it to be
/// found in the output.
fn resolve_voice(config: &Config, engine: &str, voice: Option<&str>) -> (String, Option<String>) {
    if let Some(voice) = voice {
        return (voice.to_string(), None);
    }
    // The config already answers for its own engine: the voice it names, or that engine's
    // default when it names none.
    if engine == config.tts.engine {
        return (config.voice_name().to_string(), None);
    }
    match config.tts.voice.as_deref() {
        // Nothing was named, so nothing has to be replaced: the other engine simply uses
        // its own default.
        None => (
            arioso_config::default_voice(engine)
                .unwrap_or_default()
                .to_string(),
            None,
        ),
        Some(named) => match arioso_config::default_voice(engine) {
            Some(default) => (
                default.to_string(),
                Some(format!(
                    "tts.voice is {named}, which {engine} cannot say; speaking with its default voice {default}"
                )),
            ),
            None => (named.to_string(), None),
        },
    }
}

/// The shipped dictionary and the user's entries, read once for either engine.
fn dictionaries(
    config: &Config,
    config_path: &Path,
    dict: Option<&Path>,
) -> Result<(CmuDict, arioso_core::UserDictionary)> {
    let dir = models_dir(config);
    let dict_path = dict.map_or_else(|| dir.join("cmudict.dict"), Path::to_path_buf);
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
    Ok((dictionary, user))
}

/// Speak through Piper: one model per voice.
fn say_piper(
    config: &Config,
    request: &SayRequest<'_>,
    voice: &str,
    dictionary: &CmuDict,
    user: &arioso_core::UserDictionary,
) -> Result<()> {
    let manifest = load_manifest(config)?;
    let dir = models_dir(config);
    let voices: Vec<&ModelSpec> = manifest
        .models
        .iter()
        .filter(|m| m.kind == arioso_models::Kind::Tts && m.engine == "piper")
        .collect();
    let model = manifest.find_voice(voice).with_context(|| {
        let available = voices
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("no piper voice matches {voice:?}; available: {available}")
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

    let mut voice = PiperVoice::load(&onnx, &voice_config)?;
    // Converted as it arrives: holding the audio as floats and again as bytes costs six
    // bytes a sample for a document that can run for minutes.
    let mut pcm: Vec<u8> = Vec::new();
    let options = arioso_tts_piper::RenderOptions {
        speed: config.tts.speed,
        normalize: arioso_core::NormalizeOptions {
            time_of_day: config.tts.time_of_day,
        },
        sid: request.sid.unwrap_or(config.tts.sid),
    };
    let report = voice.render(request.text, dictionary, user, options, |audio, _| {
        arioso_tts_piper::wav::write_pcm_s16le(audio, &mut pcm);
        Ok(())
    })?;

    if pcm.is_empty() {
        bail!("nothing to say: none of the words are in the dictionary");
    }
    write_output(request.out.clone(), &pcm, voice.sample_rate())?;
    report_problems(&report.missing, &report.guessed, &report.unusable);
    Ok(())
}

/// Speak through Kokoro: one model and a voice style.
fn say_kokoro(
    config: &Config,
    request: &SayRequest<'_>,
    voice: &str,
    dictionary: &CmuDict,
    user: &arioso_core::UserDictionary,
) -> Result<()> {
    let manifest = load_manifest(config)?;
    let dir = models_dir(config);
    let model = manifest
        .models
        .iter()
        .find(|m| m.kind == arioso_models::Kind::Tts && m.engine == "kokoro")
        .with_context(
            || "no kokoro model in the catalog; run `arioso models download kokoro-82m`",
        )?;
    let file = |suffix: &str| {
        model
            .files
            .iter()
            .find(|f| f.name.ends_with(suffix))
            .map(|f| dir.join(&f.name))
            .with_context(|| format!("model {} has no {suffix} file", model.id))
    };
    let onnx = file(".onnx")?;
    let voices = file(".bin")?;
    let symbols = file("config.json")?;

    let mut voice = KokoroVoice::load(&onnx, &voices, &symbols, voice)?;
    let mut pcm: Vec<u8> = Vec::new();
    let options = arioso_tts_kokoro::RenderOptions {
        speed: config.tts.speed,
        normalize: arioso_core::NormalizeOptions {
            time_of_day: config.tts.time_of_day,
        },
    };
    let report = voice.render(request.text, dictionary, user, options, |audio, _| {
        arioso_tts_piper::wav::write_pcm_s16le(audio, &mut pcm);
        Ok(())
    })?;

    if pcm.is_empty() {
        bail!("nothing to say: none of the words are in the dictionary");
    }
    write_output(request.out.clone(), &pcm, voice.sample_rate())?;
    report_problems(&report.missing, &report.guessed, &report.unusable);
    Ok(())
}

/// Write the PCM to a WAV and say how long it is.
fn write_output(out: Option<PathBuf>, pcm: &[u8], rate: u32) -> Result<()> {
    let out = out.unwrap_or_else(|| PathBuf::from("out.wav"));
    arioso_tts_piper::wav::write_wav_pcm(&out, pcm, rate)?;
    println!(
        "wrote {} ({:.2} s at {rate} Hz)",
        out.display(),
        (pcm.len() / 2) as f32 / rate as f32
    );
    Ok(())
}

/// Say what the render could not pronounce, in the words the caller understands.
///
/// Three lists rather than a report, because each engine has its own report type and
/// the three names are all the printing needs.
fn report_problems(missing: &[String], guessed: &[String], unusable: &[String]) {
    let sample = |words: &[String]| {
        let shown = words.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
        let more = if words.len() > 5 { ", ..." } else { "" };
        format!("{shown}{more}")
    };
    if !missing.is_empty() {
        eprintln!(
            "{} word(s) not in the dictionary and skipped: {}",
            missing.len(),
            sample(missing)
        );
    }
    if !guessed.is_empty() {
        eprintln!(
            "{} word(s) not in the dictionary, spoken by spelling rules: {}",
            guessed.len(),
            sample(guessed)
        );
    }
    if !unusable.is_empty() {
        eprintln!(
            "{} entr(y/ies) the voice cannot say: {}",
            unusable.len(),
            unusable.join(", ")
        );
    }
}

/// The facts `doctor` reports about the configured certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CertificateFacts {
    /// The subject common name, when the certificate carries one.
    common_name: Option<String>,
    /// The first and last instant the certificate is valid, as absolute UTC dates.
    not_before: String,
    not_after: String,
    /// Seconds between the not-after instant and the moment `doctor` ran.
    seconds_remaining: i64,
}

/// How much life a certificate has left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CertVerdict {
    /// Thirty days or more.
    Valid,
    /// Fewer than thirty days, but not yet past the not-after date.
    Expiring,
    /// Past the not-after date.
    Expired,
}

/// A certificate closer than this to its not-after date is a warning.
const CERT_WARNING_DAYS: i64 = 30;

impl CertificateFacts {
    /// Whole days left, truncated toward zero. The sign is the verdict; the verdict itself
    /// is decided from seconds, so a certificate that lapsed an hour ago is still expired.
    fn days_remaining(&self) -> i64 {
        self.seconds_remaining / SECONDS_PER_DAY as i64
    }

    fn verdict(&self) -> CertVerdict {
        if self.seconds_remaining < 0 {
            CertVerdict::Expired
        } else if self.seconds_remaining < CERT_WARNING_DAYS * SECONDS_PER_DAY as i64 {
            CertVerdict::Expiring
        } else {
            CertVerdict::Valid
        }
    }
}

/// Read the certificate at `path`, judged against `now` in seconds since the Unix epoch.
///
/// The error names the path: an operator with several certificates on disk needs to know
/// which one `doctor` could not read.
fn read_certificate(path: &Path, now: i64) -> std::result::Result<CertificateFacts, String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    let not_a_certificate =
        |error: &dyn std::fmt::Display| format!("{} is not a certificate: {error}", path.display());
    // PEM is what the server is configured with, but a DER file is a certificate too and
    // refusing it would make `doctor` disagree with a server that starts.
    let (not_before, not_after, common_name) = match x509_parser::pem::parse_x509_pem(&bytes) {
        Ok((_, pem)) => {
            let certificate = pem
                .parse_x509()
                .map_err(|error| not_a_certificate(&error))?;
            certificate_times(&certificate)
        }
        Err(_) => {
            let (_, certificate) = x509_parser::parse_x509_certificate(&bytes)
                .map_err(|error| not_a_certificate(&error))?;
            certificate_times(&certificate)
        }
    };
    Ok(CertificateFacts {
        common_name,
        not_before: format_certificate_time(not_before),
        not_after: format_certificate_time(not_after),
        seconds_remaining: not_after.timestamp() - now,
    })
}

/// The validity bounds and subject common name, copied out of a parsed certificate.
///
/// Copied rather than returned as references: the parsed certificate borrows the buffer
/// that held the PEM, which does not outlive the parse.
fn certificate_times(
    certificate: &x509_parser::certificate::X509Certificate<'_>,
) -> (
    x509_parser::time::ASN1Time,
    x509_parser::time::ASN1Time,
    Option<String>,
) {
    let common_name = certificate
        .subject()
        .iter_common_name()
        .next()
        .and_then(|name| name.as_str().ok())
        .map(str::to_owned);
    (
        certificate.validity().not_before,
        certificate.validity().not_after,
        common_name,
    )
}

/// An ASN.1 time as an absolute UTC instant, to the minute.
///
/// The date is what an operator reading a log months later needs; the seconds are noise
/// and would make two reports of the same certificate differ for no reason.
fn format_certificate_time(time: x509_parser::time::ASN1Time) -> String {
    let datetime = time.to_datetime();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        datetime.year(),
        u8::from(datetime.month()),
        datetime.day(),
        datetime.hour(),
        datetime.minute()
    )
}

/// Report the configured certificate. Returns `(problems, warnings)` for `doctor`.
fn report_certificate(path: &Path, now: i64) -> (usize, usize) {
    let facts = match read_certificate(path, now) {
        Ok(facts) => facts,
        Err(error) => {
            println!("tls cert    {error}");
            return (1, 0);
        }
    };
    println!("tls cert    {}", path.display());
    println!(
        "  subject   {}",
        facts.common_name.as_deref().unwrap_or("(no common name)")
    );
    println!("  valid     {} to {}", facts.not_before, facts.not_after);
    let days = facts.days_remaining();
    match facts.verdict() {
        CertVerdict::Valid => {
            println!("  expires   in {days} day(s)");
            (0, 0)
        }
        CertVerdict::Expiring => {
            if days <= 0 {
                println!("  WARNING: expires today");
            } else {
                println!("  WARNING: expires in {days} day(s)");
            }
            (0, 1)
        }
        CertVerdict::Expired => {
            if days == 0 {
                println!("  EXPIRED today");
            } else {
                println!("  EXPIRED {} day(s) ago", -days);
            }
            (1, 0)
        }
    }
}

/// Seconds since the Unix epoch, for judging a certificate against the wall clock.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

fn doctor(config: &Config, config_path: &Path, strict: bool) -> Result<()> {
    let mut problems = 0usize;
    let mut warnings = 0usize;
    println!("config      {}", config_path.display());
    match config.validate() {
        Ok(()) => println!("  valid"),
        Err(e) => {
            problems += 1;
            println!("  INVALID: {e}");
        }
    }

    if let Some(cert) = &config.server.tls.cert {
        let (cert_problems, cert_warnings) = report_certificate(cert, unix_now());
        problems += cert_problems;
        warnings += cert_warnings;
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
        let signature = signature_path(&manifest);
        match Manifest::load_checked(&manifest, &signature, RELEASE_PUBKEY_HEX) {
            Ok(checked) => {
                let installed = checked
                    .manifest
                    .models
                    .iter()
                    .filter(|spec| model_installed(spec, &dir))
                    .count();
                println!(
                    "manifest    {} models, {installed} installed",
                    checked.manifest.models.len()
                );
                match checked.signature {
                    SignatureState::Verified => {
                        println!("signature   verified against the compiled release key");
                    }
                    SignatureState::Unsigned => {
                        println!(
                            "signature   unsigned: no release key is compiled into this build"
                        );
                    }
                }
            }
            Err(ManifestError::SignatureMissing { path }) => {
                problems += 1;
                println!("signature   MISSING: {}", path.display());
                println!("            a release build refuses a manifest it cannot verify");
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
    if strict && warnings > 0 {
        bail!("{warnings} warning(s) found and --strict was passed");
    }
    println!("ok");
    Ok(())
}

/// The config file of the configured voice, when that voice is on disk.
///
/// The catalog names the file, so a voice that is merely listed resolves here even before
/// anything is downloaded. Resolving is not installing: the path comes back only when the
/// file is there, because the caller reads it and a fresh install that has a dictionary
/// must not abort `doctor` over a voice nobody has fetched yet.
fn voice_config_path(config: &Config, models: &Path) -> Option<PathBuf> {
    let manifest = load_manifest(config).ok()?;
    let file = manifest
        .find_voice(config.voice_name())?
        .files
        .iter()
        .find(|file| file.name.ends_with(".onnx.json"))?;
    let path = models.join(&file.name);
    path.is_file().then_some(path)
}

/// The config of the configured Kokoro model, when it is on disk.
fn kokoro_config_path(config: &Config, models: &Path) -> Option<PathBuf> {
    let manifest = load_manifest(config).ok()?;
    let file = manifest
        .models
        .iter()
        .find(|model| model.engine == "kokoro" && model.kind == arioso_models::Kind::Tts)?
        .files
        .iter()
        .find(|file| file.name.ends_with("config.json"))?;
    let path = models.join(&file.name);
    path.is_file().then_some(path)
}

/// What a voice can say, in the form its engine checks a spelling against.
enum Inventory {
    /// A Piper voice's phoneme table.
    Piper(arioso_g2p::PhonemeMap),
    /// Kokoro's symbol table.
    Kokoro(arioso_tts_kokoro::Vocab),
}

impl Inventory {
    /// Whether this voice can say `form`, spelling it by rule only when asked.
    fn can_say(
        &self,
        form: &str,
        dictionary: &CmuDict,
        options: arioso_core::NormalizeOptions,
        rules: bool,
    ) -> bool {
        match self {
            Self::Piper(map) => form_symbols(form, map, dictionary, options, rules).is_some(),
            Self::Kokoro(vocab) => {
                let symbols = arioso_g2p::phonemes_for_form(form, dictionary, options, rules);
                !symbols.is_empty()
                    && symbols.iter().all(|symbol| {
                        symbol
                            .chars()
                            .all(|character| vocab.id(character).is_some())
                    })
            }
        }
    }
}

/// The inventory of the configured voice, or `None` when its files are not installed.
fn voice_inventory(config: &Config, models: &Path) -> Result<Option<Inventory>> {
    if config.tts.engine == "kokoro" {
        let Some(path) = kokoro_config_path(config, models) else {
            return Ok(None);
        };
        let text = std::fs::read_to_string(&path)?;
        return Ok(Some(Inventory::Kokoro(
            arioso_tts_kokoro::Vocab::from_config(&text)?,
        )));
    }
    let Some(path) = voice_config_path(config, models) else {
        return Ok(None);
    };
    let map = arioso_g2p::PhonemeMap::from_voice_config(&std::fs::read_to_string(&path)?)?;
    Ok(Some(Inventory::Piper(map)))
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

    // The voice's symbol inventory lives in its config, so no model is loaded. The catalog
    // names the file: a directory scan matched nothing when `tts.voice` was a catalog id
    // rather than the bare name, and reported a voice that was installed as missing.
    let Some(inventory) = voice_inventory(config, models)? else {
        println!(
            "            voice {} is not installed, so entries were not checked",
            config.voice_name()
        );
        return Ok(0);
    };

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
            entry
                .forms()
                .iter()
                .any(|form| inventory.can_say(form, &pronunciations, options, rules))
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
            config.voice_name()
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
/// model still loads the right one. Without a manifest the loader falls back to
/// searching, which refuses a directory holding files from two models. A manifest that is
/// there but will not load is an error: it is the file that says which files belong to the
/// model and that they are the ones signed for.
fn load_engine(
    dir: &Path,
    model: &str,
    language: Option<&str>,
    threads: usize,
) -> Result<arioso_stt_sherpa::Engine> {
    let manifest_path = dir.join("manifest.json");
    if manifest_path.exists() {
        let manifest = Manifest::load(&manifest_path).with_context(|| {
            format!("reading the model manifest at {}", manifest_path.display())
        })?;
        let spec = manifest
            .models
            .iter()
            .find(|spec| spec.id == model)
            .with_context(|| {
                format!(
                    "{model} is not named in the model manifest at {}",
                    manifest_path.display()
                )
            })?;
        let paths: Vec<std::path::PathBuf> = spec.files.iter().map(|f| dir.join(&f.name)).collect();
        let model_type = spec
            .meta
            .get("model_type")
            .and_then(serde_json::Value::as_str);
        return Ok(arioso_stt_sherpa::Engine::from_manifest_files(
            dir, &paths, model_type, language, threads,
        )?);
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
    } else if text.trim().is_empty() {
        // No speech is a result, not a failure, but a blank line reads as one. The note goes
        // to stderr so a script reading stdout still gets the empty transcript it asked for.
        eprintln!("no speech found in {}", file.display());
    } else {
        println!("{text}");
    }
    Ok(())
}

const SECONDS_PER_DAY: u64 = 86_400;

/// A log file that rolls over at midnight UTC and keeps a fixed number of older files.
///
/// The active file is exactly the path the config names, so an operator who tails
/// `log.file` keeps tailing it across a rollover. Yesterday's records move to a sibling
/// named `<file>.<YYYY-MM-DD>`, which is what `log.retention_days` prunes.
struct RotatingLog {
    state: std::sync::Mutex<OpenLog>,
}

struct OpenLog {
    path: PathBuf,
    day: u64,
    retention_days: u32,
    file: std::fs::File,
}

impl RotatingLog {
    /// Open the file, or report why it could not be opened.
    fn open(path: PathBuf, retention_days: u32) -> std::io::Result<Self> {
        // A file left by an earlier run rolls on the first write of today, so the date in
        // its name is the day its records were written, not the day it was reopened.
        let day = std::fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or_else(today, |since| since.as_secs() / SECONDS_PER_DAY);
        let log = Self::with_day(path.clone(), retention_days, day)?;
        // Pruning used to happen only inside a roll, so a process that started after a gap
        // never pruned anything and the files from the days it was not running stayed
        // forever. One pass here is what makes the window hold across restarts.
        prune_old_logs(&path, today(), retention_days);
        Ok(log)
    }

    fn with_day(path: PathBuf, retention_days: u32, day: u64) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Self {
            state: std::sync::Mutex::new(OpenLog {
                path,
                day,
                retention_days,
                file,
            }),
        })
    }

    /// Write one record, rolling first when the day changed.
    fn write_at(&self, buf: &[u8], today: u64) -> std::io::Result<usize> {
        let mut open = match self.state.lock() {
            Ok(guard) => guard,
            // A panic elsewhere must not cost the log: the records are still wanted.
            Err(poisoned) => poisoned.into_inner(),
        };
        open.roll(today);
        open.file.write(buf)
    }
}

impl std::io::Write for &RotatingLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_at(buf, today())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.state.lock() {
            Ok(mut open) => open.file.flush(),
            Err(poisoned) => poisoned.into_inner().file.flush(),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RotatingLog {
    type Writer = &'a RotatingLog;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }
}

impl OpenLog {
    fn roll(&mut self, today: u64) {
        if today == self.day {
            return;
        }
        let _ = self.file.flush();
        let rotated = self.rotated_path();
        if let Err(error) = std::fs::rename(&self.path, &rotated) {
            // Keep writing to the handle already open. A log that stops because a rename
            // failed is worse than one that is a day late in rotating, but it must say so.
            eprintln!(
                "log rotation to {} failed ({error}); still writing to {}",
                rotated.display(),
                self.path.display()
            );
            self.day = today;
            return;
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => self.file = file,
            Err(error) => eprintln!(
                "log file {} could not be reopened after rotation ({error})",
                self.path.display()
            ),
        }
        self.day = today;
        prune_old_logs(&self.path, today, self.retention_days);
    }

    /// Where this day's records go when the file rolls.
    fn rotated_path(&self) -> PathBuf {
        let base = format!("{}.{}", self.path.to_string_lossy(), date_of(self.day));
        if !Path::new(&base).exists() {
            return PathBuf::from(base);
        }
        // Never overwrite: a second process on one path, or a clock that went backwards,
        // must not cost a file that is already there.
        for n in 1..1000 {
            let candidate = format!("{base}-{n}");
            if !Path::new(&candidate).exists() {
                return PathBuf::from(candidate);
            }
        }
        PathBuf::from(format!("{base}-overflow"))
    }
}

/// Remove rotated logs older than the window.
///
/// A file counts as rotated only when it is `<file>.YYYY-MM-DD` or
/// `<file>.YYYY-MM-DD-<n>`. The suffix is parsed rather than globbed, because a loose
/// prefix also matches `arioso.log.json` and a neighbour that is not a log.
fn prune_old_logs(path: &Path, today: u64, retention_days: u32) {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        let Some(suffix) = file_name
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('.'))
        else {
            continue;
        };
        let Some(date) = suffix.get(..10) else {
            continue;
        };
        if suffix.len() > 10 && !suffix[10..].starts_with('-') {
            continue;
        }
        let Some(day) = day_from_date(date) else {
            continue;
        };
        if today.saturating_sub(day) > u64::from(retention_days) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn today() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() / SECONDS_PER_DAY)
}

/// `YYYY-MM-DD` for a day counted from 1970-01-01, in UTC.
fn date_of(day: u64) -> String {
    let (year, month, day_of_month) = civil_from_days(day);
    format!("{year:04}-{month:02}-{day_of_month:02}")
}

/// The day number of a `YYYY-MM-DD` date, when it is one.
fn day_from_date(text: &str) -> Option<u64> {
    let mut parts = text.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    u64::try_from(days_from_civil(year, month, day)).ok()
}

/// The calendar date of a day number. Howard Hinnant's civil algorithm: exact integer
/// arithmetic, no dependency, valid for every day this program will ever see.
fn civil_from_days(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// The day number of a civil date, the inverse of [`civil_from_days`].
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn init_tracing(config: &Config) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(config.log.level.clone()));
    // stderr unless the config names a file. stdout belongs to the caller: in sidecar mode
    // it carries the port line and nothing else, and a log line on that pipe would be read
    // as the port.
    match &config.log.file {
        Some(path) => match RotatingLog::open(path.clone(), config.log.retention_days) {
            Ok(writer) => {
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
    model.is_installed(dir)
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

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = include_str!("../../../models/manifest.json");

    /// A fresh install: the catalog is there, the voice it names is not.
    fn fresh_install() -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manifest.json"), MANIFEST).unwrap();
        let mut config = Config::default();
        config.models.dir = Some(dir.path().to_path_buf());
        config.tts.voice = Some("piper-en_US-lessac-medium".to_owned());
        (dir, config)
    }

    #[test]
    fn a_voice_is_taken_from_the_flag_then_the_config_then_the_engine() {
        let (_dir, mut config) = fresh_install();
        config.tts.engine = "piper".to_owned();
        config.tts.voice = Some("en_US-joe-medium".to_owned());

        // The flag wins.
        let (voice, note) = resolve_voice(&config, "piper", Some("en_GB-alba-medium"));
        assert_eq!(voice, "en_GB-alba-medium");
        assert_eq!(note, None);

        // Same engine, no flag: the config's voice.
        let (voice, note) = resolve_voice(&config, "piper", None);
        assert_eq!(voice, "en_US-joe-medium");
        assert_eq!(note, None);

        // Another engine, no flag: that engine's default, said out loud rather than
        // left to be discovered from the output.
        let (voice, note) = resolve_voice(&config, "kokoro", None);
        assert_eq!(voice, "af_heart");
        let note = note.expect("a change of voice is said, not silent");
        assert!(note.contains("af_heart"), "{note}");
        assert!(note.contains("en_US-joe-medium"), "{note}");

        // And back: the Kokoro config's voice is not one Piper can say.
        config.tts.engine = "kokoro".to_owned();
        config.tts.voice = Some("af_heart".to_owned());
        let (voice, note) = resolve_voice(&config, "piper", None);
        assert_eq!(voice, "en_US-joe-medium");
        assert!(note.is_some());
    }

    #[test]
    fn a_voice_that_is_listed_but_not_downloaded_has_no_config() {
        // The catalog names the voice, so it resolves before anything is fetched. Reading
        // that path aborted `doctor` with ENOENT for anyone who had a dictionary and no
        // models yet, which is the first run.
        let (dir, config) = fresh_install();
        assert_eq!(voice_config_path(&config, dir.path()), None);

        // The file the catalog names is what the check reads, and nothing else counts.
        let named = dir.path().join("en_US-lessac-medium.onnx.json");
        std::fs::write(&named, "{}").unwrap();
        assert_eq!(voice_config_path(&config, dir.path()), Some(named));
    }

    #[test]
    fn a_log_rolls_over_at_midnight_and_keeps_the_active_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arioso.log");
        let log = RotatingLog::with_day(path.clone(), 7, 20_000).unwrap();
        log.write_at(b"day one\n", 20_000).unwrap();
        log.write_at(b"day two\n", 20_001).unwrap();

        // The configured path holds today, and yesterday is a sibling named by its date.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "day two\n");
        let rotated = dir.path().join(format!("arioso.log.{}", date_of(20_000)));
        assert_eq!(std::fs::read_to_string(rotated).unwrap(), "day one\n");
    }

    #[test]
    fn a_fresh_start_prunes_rotated_logs_past_the_window() {
        // Pruning only ran on a roll, so a process that started on a new day with a file
        // it had just created never pruned: the logs from the days it was not running
        // stayed until a rollover happened to catch them, which is never.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arioso.log");
        let now = today();
        let stale = dir
            .path()
            .join(format!("arioso.log.{}", date_of(now.saturating_sub(30))));
        let recent = dir
            .path()
            .join(format!("arioso.log.{}", date_of(now.saturating_sub(1))));
        std::fs::write(&stale, "stale\n").unwrap();
        std::fs::write(&recent, "recent\n").unwrap();

        let _log = RotatingLog::open(path.clone(), 7).unwrap();

        assert!(
            !stale.exists(),
            "a log past the window survived a fresh start"
        );
        assert!(recent.exists(), "a log inside the window was removed");
        assert!(path.exists(), "the active file is there");
    }

    #[test]
    fn a_rollover_prunes_past_the_window_and_leaves_neighbours_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arioso.log");
        let start = 20_000u64;
        let log = RotatingLog::with_day(path.clone(), 7, start).unwrap();
        // A file for each of eleven days, so the window has to be enforced.
        for offset in 0..=10 {
            log.write_at(b"x\n", start + offset).unwrap();
        }
        // Files that are not this log's rotations, one of which shares its prefix.
        for name in ["unrelated.log", "arioso.log.bak", "arioso.log.json"] {
            std::fs::write(dir.path().join(name), "keep\n").unwrap();
        }
        log.write_at(b"x\n", start + 11).unwrap();

        let kept: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        let today = start + 11;
        // Seven days back is inside the window; eight is not.
        assert!(kept.contains(&format!("arioso.log.{}", date_of(today - 7))));
        assert!(!kept.contains(&format!("arioso.log.{}", date_of(today - 8))));
        assert!(kept.contains(&"arioso.log".to_owned()));
        for name in ["unrelated.log", "arioso.log.bak", "arioso.log.json"] {
            assert!(kept.contains(&name.to_owned()), "{name} was removed");
        }
    }

    #[test]
    fn a_day_number_and_its_date_are_inverses() {
        // A wrong date in a filename is a log nobody can find again.
        for day in [0u64, 1, 20_000, 20_739, 29_000] {
            assert_eq!(day_from_date(&date_of(day)), Some(day), "{day}");
        }
        assert_eq!(date_of(19_723), "2024-01-01");
        assert_eq!(date_of(20_000), "2024-10-04");
        assert_eq!(day_from_date("not-a-date"), None);
        assert_eq!(day_from_date("2024-10"), None);
    }

    #[test]
    fn a_voice_the_catalog_does_not_hold_has_no_config() {
        let (dir, mut config) = fresh_install();
        config.tts.voice = Some("no-such-voice".to_owned());
        std::fs::write(dir.path().join("en_US-lessac-medium.onnx.json"), "{}").unwrap();
        assert_eq!(voice_config_path(&config, dir.path()), None);
    }

    #[test]
    fn a_corrupt_manifest_is_reported_rather_than_scanned_past() {
        // The load was `if let Ok(manifest)`, so a manifest that would not parse was
        // dropped on the floor and the loader went looking for whatever files were in the
        // directory: an unverified model, loaded without a word about the manifest.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manifest.json"), "{\"schema_version\":").unwrap();
        std::fs::write(dir.path().join("model.onnx"), "not a model").unwrap();

        let error = load_engine(dir.path(), "sherpa-parakeet-tdt-0.6b-v2-en", None, 1)
            .expect_err("a broken manifest is an error");
        let message = format!("{error:#}");
        assert!(message.contains("manifest"), "{message}");
    }

    #[test]
    fn a_model_the_manifest_does_not_name_is_reported() {
        // The manifest is present and readable, so the files in the directory are not a
        // fallback: the catalog is the list of what may be loaded.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manifest.json"), MANIFEST).unwrap();

        let error = load_engine(dir.path(), "no-such-model", None, 1)
            .expect_err("a model the manifest does not name is an error");
        let message = format!("{error:#}");
        assert!(message.contains("no-such-model"), "{message}");
    }

    /// The instant the certificate tests judge from: 2027-01-15 08:00 UTC.
    ///
    /// Fixed rather than `now()`, so a certificate that is ten days from expiry today is
    /// ten days from expiry whenever the suite runs.
    const CERT_NOW: i64 = 1_800_000_000;

    fn cert_now() -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp(CERT_NOW).unwrap()
    }

    /// A self-signed certificate for `name`, valid from one day before [`cert_now`] until
    /// `valid_until_days` after it. A negative value makes the certificate already expired.
    fn write_certificate(dir: &tempfile::TempDir, name: &str, valid_until_days: i64) -> PathBuf {
        let now = cert_now();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(valid_until_days);
        let key = rcgen::KeyPair::generate().unwrap();
        let pem = params.self_signed(&key).unwrap().pem();
        let path = dir.path().join(format!("{name}.pem"));
        std::fs::write(&path, pem).unwrap();
        path
    }

    #[test]
    fn a_certificate_with_a_year_left_is_valid_and_reports_absolute_dates() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_certificate(&dir, "localhost", 365);
        let facts = read_certificate(&path, CERT_NOW).unwrap();

        assert_eq!(facts.common_name.as_deref(), Some("localhost"));
        assert_eq!(facts.verdict(), CertVerdict::Valid);
        assert_eq!(facts.days_remaining(), 365);
        // Both dates, so an operator reading a log in six months knows when it lapses.
        assert_eq!(facts.not_before, "2027-01-14 08:00 UTC");
        assert_eq!(facts.not_after, "2028-01-15 08:00 UTC");
    }

    #[test]
    fn a_certificate_inside_thirty_days_is_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_certificate(&dir, "localhost", 10);
        let facts = read_certificate(&path, CERT_NOW).unwrap();

        assert_eq!(facts.verdict(), CertVerdict::Expiring);
        assert_eq!(facts.days_remaining(), 10);
        assert_eq!(facts.not_after, "2027-01-25 08:00 UTC");
        // A warning is not a failure, so `doctor` counts no problem for it.
        assert_eq!(report_certificate(&path, CERT_NOW), (0, 1));
    }

    #[test]
    fn an_expired_certificate_is_a_problem() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_certificate(&dir, "localhost", -1);
        let facts = read_certificate(&path, CERT_NOW).unwrap();

        assert_eq!(facts.verdict(), CertVerdict::Expired);
        assert_eq!(facts.days_remaining(), -1);
        assert_eq!(facts.not_after, "2027-01-14 08:00 UTC");
        assert_eq!(report_certificate(&path, CERT_NOW), (1, 0));

        // An hour past the not-after date is expired even though no whole day has passed,
        // and the printed count must not round that hour up to a day.
        let mut lapsed = facts;
        lapsed.seconds_remaining = -3_600;
        assert_eq!(lapsed.verdict(), CertVerdict::Expired);
        assert_eq!(lapsed.days_remaining(), 0);
    }

    #[test]
    fn a_file_that_is_not_a_certificate_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cert.pem");
        std::fs::write(&path, "this is not a certificate\n").unwrap();

        let error = read_certificate(&path, CERT_NOW).map(|_| ()).unwrap_err();
        assert!(error.contains(&path.display().to_string()), "{error}");
        assert_eq!(report_certificate(&path, CERT_NOW), (1, 0));

        // A path that is not there is the same verdict, and also names the file.
        let missing = dir.path().join("absent.pem");
        let error = read_certificate(&missing, CERT_NOW)
            .map(|_| ())
            .unwrap_err();
        assert!(error.contains(&missing.display().to_string()), "{error}");
    }
}
