use anyhow::Result;
use fern::colors::{Color, ColoredLevelConfig};
use log::LevelFilter;

fn colorize_metrics(input: &str) -> String {
    input
        .split_whitespace()
        .map(|token| {
            if token.starts_with("block=") {
                format!("\x1b[96m{}\x1b[0m", token)
            } else {
                token.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn setup_logger() -> Result<()> {
    let colors = ColoredLevelConfig {
        trace: Color::Cyan,
        debug: Color::Magenta,
        info: Color::Green,
        warn: Color::Red,
        error: Color::BrightRed,
        ..ColoredLevelConfig::new()
    };

    let app_level = match std::env::var("BOT_LOG_LEVEL")
        .unwrap_or_else(|_| "info".to_string())
        .to_ascii_lowercase()
        .as_str()
    {
        "trace" => LevelFilter::Trace,
        "debug" => LevelFilter::Debug,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        _ => LevelFilter::Info,
    };

    let chain_name = std::env::var("BOT_CHAIN")
        .unwrap_or_else(|_| "base".to_string())
        .to_ascii_lowercase();
    let bot_dir = format!("bot/{chain_name}");
    std::fs::create_dir_all(&bot_dir)?;
    let runtime_log = fern::log_file(format!("{bot_dir}/runtime.log"))?;

    let mut dispatch = fern::Dispatch::new()
        .format(move |out, message, record| {
            let ts = chrono::Local::now().format("%H:%M:%S%.3f");
            let msg = colorize_metrics(&message.to_string());
            out.finish(format_args!(
                "\x1b[94m[{}]\x1b[0m[{}] {}",
                ts,
                colors.color(record.level()),
                msg
            ))
        })
        .chain(std::io::stdout())
        .chain(runtime_log)
        .level(log::LevelFilter::Error)
        .level_for("rust", app_level);

    // Parse RUST_LOG for per-module level overrides (e.g.
    // RUST_LOG=info,rust::simulator_pipeline=debug).
    // Fern doesn't natively support RUST_LOG, so we emulate it here.
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        for directive in rust_log.split(',') {
            let directive = directive.trim();
            if let Some((module, level_str)) = directive.rsplit_once('=') {
                let level = match level_str.to_ascii_lowercase().as_str() {
                    "trace" => LevelFilter::Trace,
                    "debug" => LevelFilter::Debug,
                    "info" => LevelFilter::Info,
                    "warn" => LevelFilter::Warn,
                    "error" => LevelFilter::Error,
                    "off" => LevelFilter::Off,
                    _ => continue,
                };
                // Convert module path separators: env_logger uses `::`
                dispatch = dispatch.level_for(module.to_string(), level);
            }
        }
    }

    dispatch.apply()?;

    Ok(())
}

