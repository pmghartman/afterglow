//! afterglow.exe <afterglow.conf>: the link served until shutdown

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use afterglow::link::{Link, VERSION};
use afterglow::{conf, error, info, log, platform};

fn main() {
    let arg = std::env::args().nth(1);
    let Some(path) = arg.filter(|a| a != "--help") else {
        eprintln!("usage: afterglow <afterglow.conf>\n       afterglow --version");
        std::process::exit(2);
    };
    if path == "--version" {
        println!("afterglow {VERSION}");
        return;
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
