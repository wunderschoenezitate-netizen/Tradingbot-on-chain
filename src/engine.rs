//! Trading-Engine: besitzt den Grid-State exklusiv (Actor-Pattern, keine Locks) und
//! verarbeitet jeden Tick ohne `await` auf I/O – Discord-Alerts gehen per `try_send`
//! in eine Queue, damit ein langsamer Webhook nie die Execution bremst.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use tokio::{sync::mpsc, time::Instant};
use tracing::{debug, info, warn};

use crate::{config::Config, grid::Grid, types::Event, types::Tick};

const STATUS_INTERVAL: Duration = Duration::from_secs(60);

pub async fn run(
    cfg: Arc<Config>,
    mut ticks: mpsc::Receiver<Tick>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    let Some(first) = ticks.recv().await else {
        info!("Beendet, bevor ein Preis empfangen wurde – kein Grid platziert");
        return Ok(());
    };
    let (mut grid, placed) = Grid::new(cfg.grid.clone(), cfg.paper_start_quote, &first)
        .context("Grid konnte nicht platziert werden")?;
    emit(&events, placed);

    let mut buf: Vec<Event> = Vec::with_capacity(16);
    let mut tick_count: u64 = 0;
    let mut last_status = Instant::now();

    while let Some(tick) = ticks.recv().await {
        grid.on_tick(&tick, &mut buf);
        tick_count += 1;

        if !buf.is_empty() {
            debug!(
                "Tick→Fill-Entscheidung in {:?} ({} Events)",
                tick.received_at.elapsed(),
                buf.len()
            );
            for ev in buf.drain(..) {
                emit(&events, ev);
            }
        }

        if last_status.elapsed() >= STATUS_INTERVAL {
            let s = grid.stats();
            info!(
                "Status: {tick_count} Ticks/{:?} · mid={:.4} · PnL={:+.4} · Equity={:.2} · Fills {}B/{}S",
                STATUS_INTERVAL,
                s.mark_price,
                s.realized_pnl,
                s.equity(),
                s.buy_fills,
                s.sell_fills
            );
            tick_count = 0;
            last_status = Instant::now();
        }
    }

    // Feed geschlossen (Shutdown) → Abschluss-Report garantiert zustellen.
    let _ = events
        .send(Event::Shutdown {
            stats: grid.stats(),
        })
        .await;
    Ok(())
}

fn emit(tx: &mpsc::Sender<Event>, ev: Event) {
    if let Err(e) = tx.try_send(ev) {
        warn!("Discord-Queue voll oder geschlossen – Event verworfen: {e}");
    }
}
