//! SOL/USDC Grid-Trading-Bot.
//!
//! Task-Topologie (tokio Multi-Thread-Runtime):
//!
//! ```text
//!  [feed]  ──Tick (mpsc)──▶  [engine]  ──Event (mpsc)──▶  [discord]
//!  Websocket, Reconnect      Grid + Paper-Fills          Webhook, Batching, Rate-Limits
//! ```

mod config;
mod discord;
mod engine;
mod feed;
mod grid;
mod types;

use std::{process::ExitCode, sync::Arc, time::Duration};

use anyhow::{bail, Context, Result};
use tokio::{sync::mpsc, time::timeout};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::config::Config;

const TICK_QUEUE: usize = 4096;
const EVENT_QUEUE: usize = 10_000;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    if let Err(e) = load_dotenv() {
        eprintln!("Fehler in .env: {e:#}");
        return ExitCode::FAILURE;
    }
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cfg = Arc::new(Config::from_env().context("Konfiguration ungültig")?);
    if !cfg.dry_run {
        bail!(
            "DRY_RUN=false: Live-Order-Ausführung ist in dieser Version bewusst nicht \
             implementiert. Bitte zuerst im Paper-Modus testen (DRY_RUN=true)."
        );
    }
    let g = &cfg.grid;
    info!(
        "Start: {} {} · PAPER · {} Levels/Seite · Spacing {:.3} % (min {:.3} %) · {} Quote/Level",
        cfg.exchange,
        cfg.symbol,
        g.levels_per_side,
        g.spacing * 100.0,
        g.min_spacing() * 100.0,
        g.order_size_quote
    );

    let (tick_tx, tick_rx) = mpsc::channel(TICK_QUEUE);
    let (event_tx, event_rx) = mpsc::channel(EVENT_QUEUE);

    let feed = tokio::spawn(feed::run(cfg.clone(), tick_tx));
    let notifier = tokio::spawn(discord::run(cfg.clone(), event_rx));
    let mut engine = tokio::spawn(engine::run(cfg.clone(), tick_rx, event_tx));

    let finished_early = tokio::select! {
        _ = shutdown_signal() => {
            info!("Shutdown-Signal empfangen, beende sauber …");
            None
        }
        res = &mut engine => Some(res),
    };

    // Feed stoppen → Tick-Channel schließt → Engine sendet Abschluss-Report und endet
    // → Event-Channel schließt → Notifier leert seine Queue und endet.
    feed.abort();
    let engine_result = match finished_early {
        Some(res) => res,
        None => match timeout(Duration::from_secs(5), &mut engine).await {
            Ok(res) => res,
            Err(_) => {
                warn!("Engine reagiert nicht, wird abgebrochen");
                engine.abort();
                Ok(Ok(()))
            }
        },
    };
    if timeout(Duration::from_secs(20), notifier).await.is_err() {
        warn!("Discord-Queue konnte nicht vollständig geleert werden");
    }

    engine_result.context("Engine-Task ist abgestürzt")?
}

fn load_dotenv() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(e) if e.not_found() => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            error!("Ctrl-C-Handler fehlgeschlagen: {e}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                error!("SIGTERM-Handler fehlgeschlagen: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}
