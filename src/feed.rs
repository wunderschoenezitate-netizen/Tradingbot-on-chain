//! Websocket-Price-Feed (Best Bid/Ask) mit automatischem Reconnect.

use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::{
    sync::mpsc,
    time::{interval, sleep, timeout, MissedTickBehavior},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::{
    config::{Config, Exchange},
    types::Tick,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const BYBIT_PING_INTERVAL: Duration = Duration::from_secs(20);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Läuft, bis der Empfänger (Engine) geschlossen wird. Verbindungsfehler führen nie zum
/// Abbruch, sondern zu einem Reconnect mit exponentiellem Backoff.
pub async fn run(cfg: Arc<Config>, tx: mpsc::Sender<Tick>) -> Result<()> {
    let mut backoff = Duration::from_millis(250);
    loop {
        match session(&cfg, &tx, &mut backoff).await {
            Ok(()) => return Ok(()), // Engine hat den Channel geschlossen
            Err(e) => warn!("Feed-Verbindung verloren: {e:#} – Reconnect in {backoff:?}"),
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn session(cfg: &Config, tx: &mpsc::Sender<Tick>, backoff: &mut Duration) -> Result<()> {
    let (ws, _) = timeout(CONNECT_TIMEOUT, connect_async(cfg.ws_url.as_str()))
        .await
        .map_err(|_| anyhow!("Connect-Timeout nach {CONNECT_TIMEOUT:?}"))?
        .with_context(|| format!("Verbindung zu {} fehlgeschlagen", cfg.ws_url))?;
    info!("{} Websocket verbunden ({})", cfg.exchange, cfg.symbol);

    let (mut write, mut read) = ws.split();
    let is_bybit = cfg.exchange == Exchange::Bybit;
    if is_bybit {
        let sub = serde_json::json!({
            "op": "subscribe",
            "args": [format!("orderbook.1.{}", cfg.symbol)],
        });
        write.send(Message::Text(sub.to_string().into())).await?;
    }

    let mut ping = interval(BYBIT_PING_INTERVAL);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut book = TopOfBook::default();
    let mut first = true;

    loop {
        tokio::select! {
            // `biased`: Marktdaten haben immer Vorrang vor dem Ping-Timer.
            biased;
            msg = timeout(cfg.feed_timeout, read.next()) => {
                let msg = msg
                    .map_err(|_| anyhow!("keine Daten seit {:?}", cfg.feed_timeout))?
                    .ok_or_else(|| anyhow!("Stream vom Server beendet"))??;
                let received_at = std::time::Instant::now();
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(frame) => bail!("Server hat geschlossen: {frame:?}"),
                    // Pings beantwortet tungstenite automatisch.
                    _ => continue,
                };
                let parsed = match cfg.exchange {
                    Exchange::Binance => parse_binance(text.as_str()),
                    Exchange::Bybit => parse_bybit(text.as_str(), &mut book),
                };
                let Some((bid, ask)) = parsed else {
                    debug!("ignoriere Nachricht: {}", text.as_str());
                    continue;
                };
                if !(bid > 0.0 && ask >= bid) {
                    continue;
                }
                if first {
                    first = false;
                    *backoff = Duration::from_millis(250);
                    info!("erster Preis: bid={bid} ask={ask}");
                }
                if tx.send(Tick { bid, ask, received_at }).await.is_err() {
                    return Ok(());
                }
            }
            _ = ping.tick(), if is_bybit => {
                write.send(Message::Text(r#"{"op":"ping"}"#.into())).await?;
            }
        }
    }
}

/// Binance `<symbol>@bookTicker`: `{"u":..,"s":"SOLUSDC","b":"151.23","B":"..","a":"151.24","A":".."}`
fn parse_binance(text: &str) -> Option<(f64, f64)> {
    #[derive(Deserialize)]
    struct BookTicker<'a> {
        #[serde(rename = "b", borrow)]
        bid: &'a str,
        #[serde(rename = "a", borrow)]
        ask: &'a str,
    }
    let t: BookTicker = serde_json::from_str(text).ok()?;
    Some((t.bid.parse().ok()?, t.ask.parse().ok()?))
}

#[derive(Default)]
struct TopOfBook {
    bid: f64,
    ask: f64,
}

/// Bybit v5 `orderbook.1.<symbol>`: `{"topic":..,"type":"snapshot","data":{"b":[["p","q"]],"a":[["p","q"]]}}`
fn parse_bybit(text: &str, book: &mut TopOfBook) -> Option<(f64, f64)> {
    #[derive(Deserialize)]
    struct Msg<'a> {
        #[serde(borrow)]
        topic: Option<&'a str>,
        #[serde(borrow)]
        data: Option<Data<'a>>,
    }
    #[derive(Deserialize)]
    struct Data<'a> {
        #[serde(borrow, default)]
        b: Vec<(&'a str, &'a str)>,
        #[serde(borrow, default)]
        a: Vec<(&'a str, &'a str)>,
    }

    let msg: Msg = serde_json::from_str(text).ok()?;
    if !msg.topic?.starts_with("orderbook.") {
        return None;
    }
    let data = msg.data?;
    let best = |levels: &[(&str, &str)]| -> Option<f64> {
        let (px, qty) = levels.first()?;
        let qty: f64 = qty.parse().ok()?;
        (qty > 0.0).then(|| px.parse().ok()).flatten()
    };
    if let Some(bid) = best(&data.b) {
        book.bid = bid;
    }
    if let Some(ask) = best(&data.a) {
        book.ask = ask;
    }
    (book.bid > 0.0 && book.ask > 0.0).then_some((book.bid, book.ask))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binance_book_ticker() {
        let raw =
            r#"{"u":1,"s":"SOLUSDC","b":"151.23000000","B":"10.5","a":"151.24000000","A":"3.2"}"#;
        assert_eq!(parse_binance(raw), Some((151.23, 151.24)));
    }

    #[test]
    fn parses_bybit_orderbook_l1() {
        let mut book = TopOfBook::default();
        let raw = r#"{"topic":"orderbook.1.SOLUSDC","ts":1,"type":"snapshot","data":{"s":"SOLUSDC","b":[["151.2","4"]],"a":[["151.3","2"]],"u":1,"seq":1}}"#;
        assert_eq!(parse_bybit(raw, &mut book), Some((151.2, 151.3)));
        let pong = r#"{"success":true,"ret_msg":"pong","op":"ping"}"#;
        assert_eq!(parse_bybit(pong, &mut book), None);
    }
}
