//! Discord-Webhook-Notifier: eigener Task, bündelt Events zu Nachrichten mit bis zu
//! 10 Embeds und respektiert die Discord-Rate-Limits (429 / `X-RateLimit-*`).

use std::{fmt::Write as _, sync::Arc, time::Duration};

use reqwest::{header::HeaderMap, Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, time::sleep};
use tracing::{error, info, warn};

use crate::{
    config::Config,
    types::{Direction, Event, GridChange, GridSnapshot, Side, Stats, StepProfit},
};

const MAX_EMBEDS_PER_MESSAGE: usize = 10;
/// Discord erlaubt 6000 Zeichen pro Nachricht über alle Embeds; wir bleiben darunter.
const MAX_CHARS_PER_MESSAGE: usize = 5500;
const MAX_ATTEMPTS: u32 = 5;

const COLOR_BUY: u32 = 0x2ECC71;
const COLOR_SELL: u32 = 0xE74C3C;
const COLOR_PLACED: u32 = 0x3498DB;
const COLOR_ADJUSTED: u32 = 0xF1C40F;
const COLOR_WARN: u32 = 0xE67E22;
const COLOR_INFO: u32 = 0x95A5A6;

#[derive(Serialize)]
struct Payload<'a> {
    username: &'a str,
    embeds: &'a [Embed],
}

#[derive(Serialize)]
struct Embed {
    title: String,
    color: u32,
    fields: Vec<Field>,
    footer: Footer,
    timestamp: String,
}

#[derive(Serialize)]
struct Field {
    name: String,
    value: String,
    inline: bool,
}

#[derive(Serialize)]
struct Footer {
    text: String,
}

#[derive(Deserialize)]
struct RateLimited {
    retry_after: f64,
}

/// Läuft, bis alle Sender (Engine) gedroppt sind und die Queue leer ist.
pub async fn run(cfg: Arc<Config>, mut rx: mpsc::Receiver<Event>) {
    let client = match Client::builder()
        .timeout(Duration::from_secs(10))
        .tcp_nodelay(true)
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("HTTP-Client konnte nicht erstellt werden, Discord deaktiviert: {e}");
            while rx.recv().await.is_some() {}
            return;
        }
    };
    if cfg.discord_webhook_url.is_none() {
        warn!("DISCORD_WEBHOOK_URL nicht gesetzt – Events werden nur geloggt");
    }

    let mut batch: Vec<Embed> = Vec::with_capacity(MAX_EMBEDS_PER_MESSAGE);
    let mut pending: Option<Embed> = None;

    loop {
        // Erstes Event blockierend holen, weitere bereits wartende ohne Warten anhängen.
        let first = match pending.take() {
            Some(e) => e,
            None => match rx.recv().await {
                Some(ev) => build(&cfg, &ev),
                None => break,
            },
        };
        let mut chars = embed_len(&first);
        batch.push(first);
        while batch.len() < MAX_EMBEDS_PER_MESSAGE {
            let Ok(ev) = rx.try_recv() else { break };
            let embed = build(&cfg, &ev);
            let len = embed_len(&embed);
            if chars + len > MAX_CHARS_PER_MESSAGE {
                pending = Some(embed);
                break;
            }
            chars += len;
            batch.push(embed);
        }

        if let Some(url) = &cfg.discord_webhook_url {
            send(&client, url, &cfg.discord_username, &batch).await;
        }
        batch.clear();
    }
    info!("Discord-Notifier beendet");
}

async fn send(client: &Client, url: &str, username: &str, embeds: &[Embed]) {
    let payload = Payload { username, embeds };
    let mut backoff = Duration::from_millis(500);

    for attempt in 1..=MAX_ATTEMPTS {
        let result = client.post(url).json(&payload).send().await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                // Proaktiv drosseln, bevor Discord ein 429 schickt.
                if let Some(wait) = bucket_exhausted(resp.headers()) {
                    sleep(wait).await;
                }
                return;
            }
            Ok(resp) if resp.status() == StatusCode::TOO_MANY_REQUESTS => {
                let header_wait = retry_after_header(resp.headers());
                let body_wait = resp
                    .json::<RateLimited>()
                    .await
                    .ok()
                    .map(|r| Duration::from_secs_f64(r.retry_after.max(0.0)));
                let wait = body_wait.or(header_wait).unwrap_or(backoff);
                warn!("Discord Rate-Limit, warte {wait:?} (Versuch {attempt}/{MAX_ATTEMPTS})");
                sleep(wait).await;
            }
            Ok(resp) if resp.status().is_server_error() => {
                warn!("Discord {} – Retry in {backoff:?}", resp.status());
                sleep(backoff).await;
                backoff *= 2;
            }
            Ok(resp) => {
                // 4xx (außer 429) ist nicht wiederholbar, z. B. ungültige URL oder Embed.
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                error!("Discord hat die Nachricht abgelehnt ({status}): {body}");
                return;
            }
            Err(e) => {
                // URL enthält das Webhook-Token → ohne URL loggen.
                warn!(
                    "Discord-Request fehlgeschlagen: {} – Retry in {backoff:?}",
                    e.without_url()
                );
                sleep(backoff).await;
                backoff *= 2;
            }
        }
    }
    error!("Discord-Nachricht nach {MAX_ATTEMPTS} Versuchen verworfen");
}

fn bucket_exhausted(h: &HeaderMap) -> Option<Duration> {
    let remaining: u32 = h
        .get("x-ratelimit-remaining")?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    if remaining > 0 {
        return None;
    }
    let secs: f64 = h
        .get("x-ratelimit-reset-after")?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    Some(Duration::from_secs_f64(secs.max(0.0)))
}

fn retry_after_header(h: &HeaderMap) -> Option<Duration> {
    let secs: f64 = h.get("retry-after")?.to_str().ok()?.parse().ok()?;
    Some(Duration::from_secs_f64(secs.max(0.0)))
}

fn embed_len(e: &Embed) -> usize {
    e.title.len()
        + e.footer.text.len()
        + e.fields
            .iter()
            .map(|f| f.name.len() + f.value.len())
            .sum::<usize>()
}

// ---------------------------------------------------------------------------
// Embed-Aufbau
// ---------------------------------------------------------------------------

fn build(cfg: &Config, ev: &Event) -> Embed {
    log_event(ev);
    let sym = &cfg.symbol;
    let (title, color, fields) = match ev {
        Event::Fill {
            side,
            price,
            qty,
            fee,
            realized_pnl,
            counter_order,
            step_profit,
            snapshot: s,
            stats,
        } => {
            let pd = s.price_decimals;
            let (icon, color) = match side {
                Side::Buy => ("🟢", COLOR_BUY),
                Side::Sell => ("🔴", COLOR_SELL),
            };
            let mut f = vec![
                field("Trade-Typ", format!("**{side}** (Fill)"), true),
                field("Ausführungspreis", format!("`{price:.pd$}`"), true),
                field("Menge", format!("`{qty:.qd$}`", qd = s.qty_decimals), true),
                field("Gebühr", format!("`{fee:.4}`"), true),
                field(
                    "Neue Gegenorder",
                    format!("{} @ `{:.pd$}`", counter_order.0, counter_order.1),
                    true,
                ),
                field("Profit / Grid-Step (netto)", fmt_step(step_profit), true),
            ];
            if let Some(pnl) = realized_pnl {
                f.push(field(
                    "Realisiert (dieser Zyklus)",
                    format!("`{pnl:+.4}`"),
                    true,
                ));
            }
            f.extend(common_fields(s, stats));
            (
                format!("{icon} {side} FILL · {sym} @ {price:.pd$}"),
                color,
                f,
            )
        }
        Event::Grid {
            change,
            center,
            inventory_buy,
            step_profit,
            snapshot: s,
            stats,
        } => {
            let pd = s.price_decimals;
            let (title, color, mut f) = match change {
                GridChange::Placed => (
                    format!("🧱 GRID PLATZIERT · {sym} @ {center:.pd$}"),
                    COLOR_PLACED,
                    vec![field("Trade-Typ", "**PLACE** (Initial)".into(), true)],
                ),
                GridChange::Adjusted { old_low, old_high } => (
                    format!("♻️ GRID ANGEPASST · {sym} @ {center:.pd$}"),
                    COLOR_ADJUSTED,
                    vec![
                        field("Trade-Typ", "**ADJUST** (Breakout ↑)".into(), true),
                        field(
                            "Alte Range",
                            format!("`{old_low:.pd$} – {old_high:.pd$}`"),
                            true,
                        ),
                    ],
                ),
            };
            f.push(field(
                "Neue Range",
                format!("`{:.pd$} – {:.pd$}`", s.low, s.high),
                true,
            ));
            f.push(field(
                "Order-Menge / Level",
                format!("`{:.qd$}`", s.qty, qd = s.qty_decimals),
                true,
            ));
            f.push(field(
                "Profit / Grid-Step (netto)",
                fmt_step(step_profit),
                true,
            ));
            if let Some((qty, px, fee)) = inventory_buy {
                f.push(field(
                    "Inventar-Kauf (Taker)",
                    format!(
                        "`{qty:.qd$}` @ `{px:.pd$}` · Fee `{fee:.4}`",
                        qd = s.qty_decimals
                    ),
                    false,
                ));
            }
            f.extend(common_fields(s, stats));
            (title, color, f)
        }
        Event::OutOfRange {
            direction,
            price,
            snapshot: s,
        } => {
            let pd = s.price_decimals;
            let hint = match direction {
                Direction::Above => "Preis über dem Grid – alle Sells gefüllt.",
                Direction::Below => {
                    "Preis unter dem Grid – alle Buys gefüllt, Bot hält SOL und wartet auf Erholung."
                }
            };
            (
                format!("⚠️ OUT OF RANGE · {sym} @ {price:.pd$}"),
                COLOR_WARN,
                vec![
                    field("Hinweis", hint.into(), false),
                    field("Range", format!("`{:.pd$} – {:.pd$}`", s.low, s.high), true),
                    field("Aktive Grid-Ebenen", levels_summary(s), true),
                    field("Grid", grid_table(s), false),
                ],
            )
        }
        Event::Shutdown { stats } => (
            "⏹️ BOT GESTOPPT".to_string(),
            COLOR_INFO,
            vec![
                field(
                    "Realisierter PnL",
                    format!("`{:+.4}`", stats.realized_pnl),
                    true,
                ),
                field(
                    "Fills",
                    format!("{} BUY / {} SELL", stats.buy_fills, stats.sell_fills),
                    true,
                ),
                field("Equity", fmt_equity(stats), false),
            ],
        ),
    };

    let mode = if cfg.dry_run {
        "📝 PAPER-TRADING"
    } else {
        "💸 LIVE"
    };
    Embed {
        title,
        color,
        fields,
        footer: Footer {
            text: format!("{mode} · {} · {sym}", cfg.exchange),
        },
        timestamp: chrono::Utc::now().to_rfc3339(),
    }
}

fn common_fields(s: &GridSnapshot, stats: &Stats) -> [Field; 4] {
    [
        field("Aktive Grid-Ebenen", levels_summary(s), true),
        field(
            "Realisierter PnL gesamt",
            format!("`{:+.4}`", stats.realized_pnl),
            true,
        ),
        field("Equity (Mark-to-Market)", fmt_equity(stats), false),
        field("Grid", grid_table(s), false),
    ]
}

fn levels_summary(s: &GridSnapshot) -> String {
    format!("**{}** BUY / **{}** SELL", s.active_buys, s.active_sells)
}

fn fmt_step(p: &StepProfit) -> String {
    format!("`{:+.4}` ({:+.3} %)", p.quote, p.pct * 100.0)
}

fn fmt_equity(s: &Stats) -> String {
    let eq = s.equity();
    let delta = eq - s.start_equity;
    format!(
        "`{eq:.2}` ({delta:+.2}) · Quote `{:.2}` · Base `{:.4}` · Fees `{:.4}`",
        s.quote_balance, s.base_balance, s.fees_paid
    )
}

/// Kompakte Tabelle der Levels rund um den Marktpreis als Code-Block.
fn grid_table(s: &GridSnapshot) -> String {
    let pd = s.price_decimals;
    let mut out = String::from("```\n");
    for lvl in &s.window {
        let (marker, label) = match (lvl.is_cursor, lvl.order) {
            (true, _) => ("▶", "── Markt"),
            (_, Some(Side::Sell)) => (" ", "SELL"),
            (_, Some(Side::Buy)) => (" ", "BUY"),
            (_, None) => (" ", "-"),
        };
        let _ = writeln!(out, "{marker} {:>12.pd$}  {label}", lvl.price);
    }
    out.push_str("```");
    out
}

fn field(name: &str, value: String, inline: bool) -> Field {
    Field {
        name: name.to_string(),
        value,
        inline,
    }
}

fn log_event(ev: &Event) {
    match ev {
        Event::Fill {
            side,
            price,
            qty,
            realized_pnl,
            counter_order,
            ..
        } => info!(
            "FILL {side} {qty} @ {price} → {} @ {} (pnl {:?})",
            counter_order.0, counter_order.1, realized_pnl
        ),
        Event::Grid {
            change, snapshot, ..
        } => info!(
            "GRID {:?}: {} – {} ({} BUY / {} SELL)",
            change, snapshot.low, snapshot.high, snapshot.active_buys, snapshot.active_sells
        ),
        Event::OutOfRange {
            direction, price, ..
        } => warn!("OUT OF RANGE {direction:?} @ {price}"),
        Event::Shutdown { stats } => info!(
            "SHUTDOWN pnl={:+.4} equity={:.2}",
            stats.realized_pnl,
            stats.equity()
        ),
    }
}
