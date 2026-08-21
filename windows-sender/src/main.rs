//! Remote Input Bridge - Windows sender.
//!
//! Layout of the threads (spec §34):
//!   * main thread      - window, tray, Raw Input drain, low-level hooks
//!   * rib-realtime     - high-resolution timer, mouse coalescing, UDP send
//!   * rib-net          - connection lifecycle, heartbeat, reliable events
//!   * rib-net-read     - blocking reads on the control channel
//!   * rib-telemetry    - once-per-second rate sampling
//!
//! The Raw Input handler never blocks on the network: it only touches atomics and an unbounded
//! channel.

#![cfg_attr(windows, windows_subsystem = "windows")]

mod autostart;
mod config;
mod crypto;
mod input;
mod log;
mod net;
mod protocol;
mod state;
mod telemetry;
mod ui;

use config::{Config, KeyStore};

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Default)]
struct Args {
    help: bool,
    version: bool,
    console: bool,
    diagnostics: bool,
    no_tray: bool,
    show: bool,
    save: bool,
    mac_host: Option<String>,
    tcp_port: Option<u16>,
    udp_port: Option<u16>,
    interval_ms: Option<u32>,
    pair_code: Option<String>,
    log_level: Option<String>,
    no_suppress: bool,
    no_udp: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => args.help = true,
            "-V" | "--version" => args.version = true,
            "--console" => args.console = true,
            "--diagnostics" => {
                args.diagnostics = true;
                args.console = true;
            }
            "--no-tray" => args.no_tray = true,
            "--show" => args.show = true,
            "--save" => args.save = true,
            "--no-suppress" => args.no_suppress = true,
            "--no-udp" => args.no_udp = true,
            "--mac" => args.mac_host = Some(value()?),
            "--tcp-port" => args.tcp_port = Some(value()?.parse().map_err(|_| "bad TCP port")?),
            "--udp-port" => args.udp_port = Some(value()?.parse().map_err(|_| "bad UDP port")?),
            "--interval" => args.interval_ms = Some(value()?.parse().map_err(|_| "bad interval")?),
            "--pair" => args.pair_code = Some(value()?),
            "--log" => args.log_level = Some(value()?),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(args)
}

fn help() -> String {
    format!(
        "Remote Input Bridge sender {VERSION}

USAGE:
    rib-sender [OPTIONS]

OPTIONS:
    --mac <host>        Mac IP address or host name
    --tcp-port <port>   control channel port (default {tcp})
    --udp-port <port>   realtime movement port (default {udp})
    --interval <ms>     mouse aggregation interval: 1, 2, 4 or 8 (default 2)
    --pair <code>       pair with the Mac using the code it displays
    --console           attach a console and log to it
    --diagnostics       like --console, plus the per-second diagnostics line
    --show              open the status window at startup
    --no-tray           do not create a tray icon (console proof-of-concept mode)
    --no-suppress       do not suppress local Windows input while the Mac is active
    --no-udp            send movement over TCP instead of UDP (for jitter A/B tests)
    --log <level>       ERROR | WARN | INFO | DEBUG | TRACE
    --save              write the options above to config.json
    -h, --help          print this help
    -V, --version       print the version

HOTKEYS (configurable in the settings window):
    Ctrl+Alt+Left           switch input to the Mac
    Ctrl+Alt+Right          switch input back to Windows
    Ctrl+Alt+Shift+Escape   emergency: force input back to Windows

CONFIG:
    {path}
",
        tcp = config::DEFAULT_TCP_PORT,
        udp = config::DEFAULT_UDP_PORT,
        path = config::config_path().display()
    )
}

fn build_config(args: &Args) -> Config {
    let mut cfg = Config::load();
    if let Some(host) = &args.mac_host {
        cfg.mac_host = host.clone();
    }
    if let Some(port) = args.tcp_port {
        cfg.tcp_port = port;
    }
    if let Some(port) = args.udp_port {
        cfg.udp_port = port;
    }
    if let Some(ms) = args.interval_ms {
        cfg.mouse_interval_ms = ms;
    }
    if let Some(level) = &args.log_level {
        cfg.log_level = level.clone();
    }
    if args.diagnostics {
        cfg.diagnostics = true;
    }
    if args.no_suppress {
        cfg.suppress_local_input = false;
    }
    if args.no_udp {
        cfg.use_udp = false;
    }
    cfg.sanitized()
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(e) => {
            attach_console();
            println!("{e}\n\n{}", help());
            std::process::exit(2);
        }
    };
    if args.help || args.version {
        attach_console();
        println!("{}", if args.help { help() } else { format!("rib-sender {VERSION}\n") });
        return;
    }

    let cfg = build_config(&args);
    log::set_level(&cfg.log_level);
    if args.console || cfg.diagnostics {
        attach_console();
    }
    if args.save {
        if let Err(e) = cfg.save() {
            log::warn(&format!("could not write config.json: {e}"));
        }
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let st = state::init_state(cfg.clone(), KeyStore::load(), tx);
    input::set_hotkeys(&cfg);

    log::info(&format!("Remote Input Bridge sender {VERSION}"));
    if cfg.mac_host.is_empty() {
        log::warn("no Mac address configured yet - set it in the settings window or with --mac");
    }

    let net_thread = net::spawn(rx);
    spawn_telemetry();

    if let Some(code) = args.pair_code.clone() {
        st.send(net::NetMsg::Pair(code));
    }

    run(&args);

    st.force_local("sender is shutting down");
    st.send(net::NetMsg::Shutdown);
    let _ = net_thread.join();
    log::info("stopped");
}

fn spawn_telemetry() {
    std::thread::Builder::new()
        .name("rib-telemetry".into())
        .spawn(|| {
            let st = state::state();
            let mut sampler = telemetry::RateSampler::default();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let snapshot = sampler.sample(&st.tel);
                st.tel.publish(snapshot);
                if st.config().diagnostics {
                    log::info(&snapshot.render(st.link().label(), st.target().label()));
                }
                diagnose(&snapshot);
            }
        })
        .expect("cannot spawn the telemetry thread");
}

/// Says out loud when the input pipeline is dead, instead of leaving it looking like a network
/// problem. Each condition is reported once.
///
/// Deliberately keyed on lifetime totals rather than the per-second rate: a user who switches to
/// the Mac and then simply does not touch the mouse produces the same zero rate as a broken
/// pipeline, and a diagnostic that cries wolf is worse than none. "Nothing has *ever* arrived"
/// cannot be confused with "nothing is happening right now".
fn diagnose(snapshot: &telemetry::Snapshot) {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    static NO_MESSAGES: AtomicBool = AtomicBool::new(false);
    static NO_EVENTS: AtomicBool = AtomicBool::new(false);
    static NO_SUPPRESSION: AtomicBool = AtomicBool::new(false);

    let st = state::state();
    if st.target() != state::Target::RemoteMac {
        return;
    }
    let _ = snapshot;
    let messages = st.tel.wm_input_messages.load(Relaxed);
    let events = st.tel.raw_mouse_events.load(Relaxed) + st.tel.raw_kbd_events.load(Relaxed);

    if messages == 0 {
        if !NO_MESSAGES.swap(true, Relaxed) {
            log::error(
                "the Mac has the input but not a single WM_INPUT message has ever arrived: Raw \
                 Input is not being delivered to our window, so there is nothing to forward.",
            );
        }
    } else if events == 0 && !NO_EVENTS.swap(true, Relaxed) {
        log::error(
            "WM_INPUT messages arrive but not one event could ever be read out of them - the raw \
             input read path is failing.",
        );
    }

    if st.config().suppress_local_input && !input::hooks_active() && !NO_SUPPRESSION.swap(true, Relaxed)
    {
        log::warn(
            "local input suppression is not active: the low-level hooks are not installed, so \
             Windows will also act on everything sent to the Mac. Input forwarding itself is \
             unaffected - it does not depend on the hooks.",
        );
    }
}

#[cfg(windows)]
fn run(args: &Args) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, TranslateMessage, MSG,
    };

    let window = ui::window::create(!args.no_tray, args.show || args.no_tray);
    if window.is_null() {
        log::error("no window: the sender cannot receive Raw Input, aborting");
        return;
    }
    input::hooks::install();

    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    // Uninstall before anything else so a crash-on-exit can never leave input swallowed.
    input::hooks::uninstall();
}

#[cfg(not(windows))]
fn run(_args: &Args) {
    println!(
        "The sender half of Remote Input Bridge only runs on Windows.\n\
         Build it with: cargo build --release --target x86_64-pc-windows-msvc"
    );
}

#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            AllocConsole();
        }
    }
}

#[cfg(not(windows))]
fn attach_console() {}
