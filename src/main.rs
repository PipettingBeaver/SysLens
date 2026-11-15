mod app;
mod config;
mod docker;
mod gpu;
mod hints;
mod proc;
mod sysd;
mod ui;
mod util;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::{App, KeyResult};
use config::GuardConfig;
use proc::Monitor;

struct Args {
    interval: f64,
    reserve_mb: u64,
    once: bool,
    no_gpu: bool,
    guard: bool,
    /// Overrides `grace` from the config file when set.
    guard_grace: Option<f64>,
    guard_targets: Vec<String>,
    allow_system: bool,
    headless_guard: bool,
    guard_config: Option<PathBuf>,
    explain_guard: bool,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut args = Args {
            interval: 1.0,
            reserve_mb: 2048,
            once: false,
            no_gpu: false,
            guard: true,
            guard_grace: None,
            guard_targets: Vec::new(),
            allow_system: false,
            headless_guard: false,
            guard_config: None,
            explain_guard: false,
        };
        let mut iter = std::env::args().skip(1);
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--once" => args.once = true,
                "--no-gpu" => args.no_gpu = true,
                "--guard" => args.guard = true,
                "--no-guard" => args.guard = false,
                "--allow-system" => args.allow_system = true,
                "--headless-guard" => args.headless_guard = true,
                "--explain-guard" => args.explain_guard = true,
                "-h" | "--help" => {
                    print_help();
                    std::process::exit(0);
                }
                "-V" | "--version" => {
                    println!("syslens {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                "-i" | "--interval" => {
                    let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
                    args.interval = parse_interval(&value)?;
                }
                "--reserve-mb" => {
                    let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
                    args.reserve_mb = parse_reserve(&value)?;
                }
                "--guard-grace" => {
                    let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
                    args.guard_grace = Some(parse_interval(&value)?);
                }
                "--guard-target" => {
                    let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
                    push_targets(&mut args.guard_targets, &value);
                }
                "--guard-config" => {
                    let value = iter.next().ok_or_else(|| format!("{arg} needs a value"))?;
                    args.guard_config = Some(PathBuf::from(value));
                }
                other if other.starts_with("--interval=") => {
                    args.interval = parse_interval(&other["--interval=".len()..])?;
                }
                other if other.starts_with("--reserve-mb=") => {
                    args.reserve_mb = parse_reserve(&other["--reserve-mb=".len()..])?;
                }
                other if other.starts_with("--guard-grace=") => {
                    args.guard_grace = Some(parse_interval(&other["--guard-grace=".len()..])?);
                }
                other if other.starts_with("--guard-target=") => {
                    push_targets(&mut args.guard_targets, &other["--guard-target=".len()..]);
                }
                other if other.starts_with("--guard-config=") => {
                    args.guard_config = Some(PathBuf::from(&other["--guard-config=".len()..]));
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(args)
    }
}

fn parse_interval(value: &str) -> Result<f64, String> {
    let interval: f64 = value
        .parse()
        .map_err(|_| format!("invalid interval: {value}"))?;
    if !interval.is_finite() || interval <= 0.0 {
        return Err(format!("interval must be positive: {value}"));
    }
    Ok(interval)
}

fn parse_reserve(value: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("invalid reserve size: {value}"))
}

fn push_targets(targets: &mut Vec<String>, value: &str) {
    for part in value.split(',') {
        let part = part.trim();
        if !part.is_empty() {
            targets.push(part.to_string());
        }
    }
}

fn print_help() {
    println!(
        "\
SysLens {version} - lightweight per-unit CPU, RAM and GPU monitor for Linux

USAGE:
  syslens [OPTIONS]

OPTIONS:
  -i, --interval SECS   screen refresh interval (default 1.0)
      --reserve-mb MB   VRAM kept for the compositor in warnings (default 2048)
      --once            print one plain snapshot and exit
      --no-gpu          skip GPU queries
      --no-guard        disable the automatic guard for this run
      --guard-grace S   override the guard grace period (default 60, or the value in guard.conf)
      --guard-target U  only auto-guard this unit (repeatable, comma-separated)
      --guard-config P  read guard settings from P (default ~/.config/syslens/guard.conf)
      --explain-guard   print why each unit is or is not guard-eligible, then exit
      --allow-system    allow system model units (needs root or passwordless sudo)
      --headless-guard  run the guard without the TUI (for a systemd service)
  -h, --help            show this help
  -V, --version         show the version

KEYS:
  j/k or arrows   select a unit
  Enter or m      actions menu (freeze/thaw/stop/start/force kill)
  s               suspend (systemctl --user stop)
  r               resume (systemctl --user start)
  f / t           freeze / thaw (pause in memory)
  K               force kill (SIGKILL)
  p               processes for the selected unit
  !               emergency panel (stop/freeze largest, all models, guard toggle)
  x               show all units / hide idle units
  ?               help      q  quit

The guard is part of the design and on by default: when VRAM, RAM, swap or
memory-stall pressure persists it freezes the largest idle model unit, then
stops it after the grace period, and thaws it when pressure clears. It never
touches protected units (compositor, desktop, session, kernel, system). Use
--no-guard to turn it off, --explain-guard to see the gating decision, and
~/.config/syslens/guard.conf to tune thresholds, hints, allow and deny lists.

GPU memory is read from nvidia-smi when present, otherwise from sysfs VRAM
counters (amdgpu) and DRM fdinfo, which also attributes per-process VRAM.

Examples:
  syslens                      live view
  syslens --once               one-shot snapshot for scripts
  syslens --explain-guard      show guard eligibility for every unit
  syslens -i 0.5               refresh twice a second
  syslens --headless-guard     always-on guard (run under systemd --user)",
        version = env!("CARGO_PKG_VERSION")
    );
}

fn main() {
    let args = match Args::parse() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("syslens: {message}");
            std::process::exit(2);
        }
    };

    let (mut guard_cfg, config_path) = match config::load(args.guard_config.as_deref()) {
        Ok(loaded) => loaded,
        Err(message) => {
            eprintln!("syslens: {message}");
            std::process::exit(2);
        }
    };
    if let Some(grace) = args.guard_grace {
        guard_cfg.grace = Duration::from_secs_f64(grace);
    }
    if let Err(message) = config::install_hints(&guard_cfg) {
        eprintln!("syslens: {message}");
        std::process::exit(2);
    }

    if args.headless_guard {
        if args.once {
            eprintln!("syslens: --headless-guard and --once are mutually exclusive");
            std::process::exit(2);
        }
        if !args.guard {
            eprintln!("syslens: --headless-guard requires the guard (remove --no-guard)");
            std::process::exit(2);
        }
        run_headless_guard(&args, guard_cfg);
        return;
    }

    let mut monitor = Monitor::new(
        args.reserve_mb,
        !args.no_gpu,
        guard_cfg.gpu_log_pattern.clone(),
    );
    monitor.warmup();

    if args.explain_guard {
        print_explain(&args, &mut monitor, guard_cfg, config_path);
        return;
    }

    if args.once {
        let snapshot = monitor.sample();
        let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        print!("{}", ui::render_plain(&snapshot, color));
        return;
    }

    if !io::stdout().is_terminal() {
        eprintln!("syslens: stdout is not a terminal; use --once for a plain snapshot");
        std::process::exit(2);
    }

    let snapshot = monitor.sample();
    let mut app = App::new(
        snapshot,
        args.interval,
        args.guard,
        guard_cfg,
        args.guard_targets,
        args.allow_system,
    );
    app.reconcile_frozen();
    if let Err(error) = run_tui(&mut app, &mut monitor) {
        eprintln!("syslens: {error}");
        std::process::exit(1);
    }
}

fn print_explain(
    args: &Args,
    monitor: &mut Monitor,
    guard_cfg: GuardConfig,
    config_path: Option<PathBuf>,
) {
    match &config_path {
        Some(path) => println!("guard config: {}", path.display()),
        None => println!("guard config: built-in defaults (no guard.conf found)"),
    }
    println!(
        "sustain {}s · recover {}s · cooldown {}s · grace {}s · busy_cpu {:.0}% · allow_busy {}\n",
        guard_cfg.sustain.as_secs(),
        guard_cfg.recover.as_secs(),
        guard_cfg.cooldown.as_secs(),
        guard_cfg.grace.as_secs(),
        guard_cfg.busy_cpu,
        guard_cfg.allow_busy
    );
    let app = App::new(
        monitor.sample(),
        args.interval,
        args.guard,
        guard_cfg,
        args.guard_targets.clone(),
        args.allow_system,
    );
    print!("{}", app.explain_guard_table());
}

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn stop_handler(_: libc::c_int) {
    RUNNING.store(false, Ordering::Relaxed);
}

fn run_headless_guard(args: &Args, guard_cfg: GuardConfig) {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            stop_handler as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            stop_handler as *const () as libc::sighandler_t,
        );
    }

    let mut monitor = Monitor::new(
        args.reserve_mb,
        !args.no_gpu,
        guard_cfg.gpu_log_pattern.clone(),
    );
    monitor.warmup();
    let mut app = App::new(
        monitor.sample(),
        args.interval,
        true,
        guard_cfg.clone(),
        args.guard_targets.clone(),
        args.allow_system,
    );
    app.headless = true;
    app.reconcile_frozen();
    app.set_snapshot(monitor.sample());

    eprintln!(
        "syslens guard: running (interval {:.1}s, reserve {} MiB, grace {}s, sustain {}s{})",
        args.interval,
        args.reserve_mb,
        guard_cfg.grace.as_secs(),
        guard_cfg.sustain.as_secs(),
        if args.guard_targets.is_empty() {
            String::new()
        } else {
            format!(", targets {}", args.guard_targets.join(","))
        }
    );

    let tick = Duration::from_secs_f64(args.interval.max(0.5));
    let mut last_sample = Instant::now();
    while RUNNING.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
        if last_sample.elapsed() >= tick {
            app.set_snapshot(monitor.sample());
            last_sample = Instant::now();
        }
    }
    eprintln!("syslens guard: stopped");
}

fn run_tui(app: &mut App, monitor: &mut Monitor) -> io::Result<()> {
    let mut stdout = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        previous_hook(info);
    }));

    let result = event_loop(&mut terminal, app, monitor);

    terminal::disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    monitor: &mut Monitor,
) -> io::Result<()> {
    let tick = Duration::from_secs_f64(app.interval.max(0.2));
    let mut last_sample = Instant::now();
    let mut dirty = true;

    loop {
        if dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            dirty = false;
        }

        let timeout = tick.saturating_sub(last_sample.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match app.on_key(key) {
                        KeyResult::Quit => break,
                        KeyResult::Refresh => {
                            app.set_snapshot(monitor.sample());
                            last_sample = Instant::now();
                        }
                        KeyResult::Continue => {}
                    }
                    dirty = true;
                }
                Event::Resize(_, _) => dirty = true,
                _ => {}
            }
        }

        if last_sample.elapsed() >= tick {
            app.set_snapshot(monitor.sample());
            last_sample = Instant::now();
            dirty = true;
        }
    }
    Ok(())
}
