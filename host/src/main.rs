//! afterglow <afterglow.conf>: the link served until shutdown
//! afterglow --stdio: one driver on stdin and stdout (ssh's), the app ended with it
//! afterglow --icons: app icons for the specs on stdin, a JSON line each on stdout
//! afterglow --features: what this build serves, one line, for a driver probing a host

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use afterglow::link::{Link, PROTO, VERSION};
use afterglow::{conf, error, icons, info, log, platform};

/// what a driver can count on from this build; icons is Windows only
const FEATURES: &str = if cfg!(windows) {
    "stdio argv notes clipboard raise close cursor icons"
} else {
    "stdio argv notes clipboard raise close cursor"
};

fn main() {
    let arg = std::env::args().nth(1);
    let Some(path) = arg.filter(|a| a != "--help") else {
        eprintln!(
            "usage: afterglow <afterglow.conf>\n       afterglow --stdio\n       afterglow --icons < specs\n       afterglow --version | --features"
        );
        std::process::exit(2);
    };
    match path.as_str() {
        "--version" => return println!("afterglow {VERSION}"),
        "--features" => return println!("afterglow {VERSION} proto={PROTO} {FEATURES}"),
        "--stdio" => return stdio(),
        "--icons" => std::process::exit(icons::run()),
        "--grab" => std::process::exit(platform::grab()),
        _ => {}
    }
    let conf = match std::fs::read_to_string(&path)
        .map_err(|e| format!("can't read {path}: {e}"))
        .and_then(|t| conf::parse(&t))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("afterglow: {e}");
            std::process::exit(1);
        }
    };
    log::init(conf.log_level, conf.log_path.as_deref());
    info!("afterglow {VERSION}");
    let shutdown = Arc::new(AtomicBool::new(false));
    platform::on_shutdown(shutdown.clone());
    let host = match platform::open(&conf) {
        Ok(h) => h,
        Err(e) => {
            error!("{e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = Link::new(host, conf.token.clone(), shutdown).run(conf.port) {
        error!("{e}");
        std::process::exit(1);
    }
    info!("shut down");
}

/// the link on stdin and stdout: nothing but the link's bytes on stdout, the log on
/// stderr (warnings and worse, or AFTERGLOW_LOG's level) or AFTERGLOW_LOG_FILE
fn stdio() {
    let level = std::env::var("AFTERGLOW_LOG").map_or(log::Level::Warning, |v| log::Level::parse(&v));
    let file = std::env::var_os("AFTERGLOW_LOG_FILE").map(std::path::PathBuf::from);
    log::init(level, file.as_deref());
    let conf = conf::Conf::stdio();
    let shutdown = Arc::new(AtomicBool::new(false));
    platform::on_shutdown(shutdown.clone());
    let host = match platform::open(&conf) {
        Ok(h) => h,
        Err(e) => {
            error!("{e}");
            std::process::exit(1);
        }
    };
    info!("afterglow {VERSION} on stdio");
    Link::stdio(host, shutdown).serve_stdio();
}
