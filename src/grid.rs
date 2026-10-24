//! Reine, synchrone Grid-Logik inkl. Paper-Trading-Buchhaltung.
//!
//! Das Grid besteht aus `2N + 1` Preisstufen. Genau eine Stufe (der *Cursor*) ist leer
//! und liegt am Marktpreis; darunter liegen nur BUY-, darüber nur SELL-Orders. Diese
//! Invariante bleibt bei jedem Fill erhalten:
//!
//! * BUY bei `i = cursor - 1` gefüllt → SELL bei `i + 1`, Cursor wandert nach `i`.
//! * SELL bei `i = cursor + 1` gefüllt → BUY bei `i - 1`, Cursor wandert nach `i`.
//!
//! Dadurch muss pro Tick nur je eine Order auf jeder Seite geprüft werden (O(1)),
//! egal wie viele Levels das Grid hat.

use anyhow::{bail, Result};

use crate::{
    config::GridConfig,
    types::{Direction, Event, GridChange, GridSnapshot, LevelView, Side, Stats, StepProfit, Tick},
};

/// Anzahl Levels über/unter dem Cursor in der Discord-Tabelle.
const SNAPSHOT_RADIUS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Slot {
    Empty,
    Buy,
    /// `cost_basis` = Einstandspreis je Einheit inkl. Kauf-Fee.
    Sell {
        cost_basis: f64,
    },
}

#[derive(Debug)]
pub struct Grid {
    cfg: GridConfig,
    prices: Vec<f64>,
    slots: Vec<Slot>,
    cursor: usize,
    qty: f64,
    price_decimals: usize,
    qty_decimals: usize,

    quote: f64,
    base: f64,
    realized_pnl: f64,
    fees_paid: f64,
    buy_fills: u64,
    sell_fills: u64,
    start_equity: f64,
    mark_price: f64,
    out_of_range: Option<Direction>,
}

impl Grid {
    /// Baut das Grid um den Mid-Preis des ersten Ticks und kauft (simuliert, Taker)
    /// das Basis-Asset für die Sell-Seite.
    pub fn new(cfg: GridConfig, start_quote: f64, tick: &Tick) -> Result<(Self, Event)> {
        let mut grid = Self {
            price_decimals: decimals(cfg.price_tick),
            qty_decimals: decimals(cfg.qty_step),
            cfg,
            prices: Vec::new(),
            slots: Vec::new(),
            cursor: 0,
            qty: 0.0,
            quote: start_quote,
            base: 0.0,
            realized_pnl: 0.0,
            fees_paid: 0.0,
            buy_fills: 0,
            sell_fills: 0,
            start_equity: start_quote,
            mark_price: tick.mid(),
            out_of_range: None,
        };
        let event = grid.place(tick, GridChange::Placed)?;
        Ok((grid, event))
    }

    /// Verarbeitet einen Tick und hängt alle resultierenden Events an `events` an.
    pub fn on_tick(&mut self, tick: &Tick, events: &mut Vec<Event>) {
        self.mark_price = tick.mid();

        // Mehrere Levels können in einem Tick gekreuzt werden (Preissprung).
        loop {
            if self.cursor > 0 {
                let i = self.cursor - 1;
                if self.slots[i] == Slot::Buy && tick.ask <= self.prices[i] {
                    events.push(self.fill_buy(i));
                    continue;
                }
            }
            if self.cursor + 1 < self.prices.len() {
                let i = self.cursor + 1;
                if let Slot::Sell { cost_basis } = self.slots[i] {
                    if tick.bid >= self.prices[i] {
                        events.push(self.fill_sell(i, cost_basis));
                        continue;
                    }
                }
            }
            break;
        }

        self.check_range(tick, events);
    }

    pub fn stats(&self) -> Stats {
        Stats {
            realized_pnl: self.realized_pnl,
            fees_paid: self.fees_paid,
            buy_fills: self.buy_fills,
            sell_fills: self.sell_fills,
            quote_balance: self.quote,
            base_balance: self.base,
            mark_price: self.mark_price,
            start_equity: self.start_equity,
        }
    }

    /// Netto-Gewinn eines Zyklus BUY@`prices[i]` → SELL@`prices[i + 1]` (Maker auf beiden Seiten).
    pub fn step_profit(&self, i: usize) -> StepProfit {
        let i = i.min(self.prices.len().saturating_sub(2));
        net_step(
            self.prices[i],
            self.prices[i + 1],
            self.qty,
            self.cfg.maker_fee,
        )
    }

    fn fill_buy(&mut self, i: usize) -> Event {
        let price = self.prices[i];
        let notional = self.qty * price;
        let fee = notional * self.cfg.maker_fee;

        self.quote -= notional + fee;
        self.base += self.qty;
        self.fees_paid += fee;
        self.buy_fills += 1;

        self.slots[i] = Slot::Empty;
        self.slots[i + 1] = Slot::Sell {
            cost_basis: price * (1.0 + self.cfg.maker_fee),
        };
        self.cursor = i;
        self.out_of_range = None;

        Event::Fill {
            side: Side::Buy,
            price,
            qty: self.qty,
            fee,
            realized_pnl: None,
            counter_order: (Side::Sell, self.prices[i + 1]),
            step_profit: self.step_profit(i),
            snapshot: self.snapshot(),
            stats: self.stats(),
        }
    }

    fn fill_sell(&mut self, i: usize, cost_basis: f64) -> Event {
        let price = self.prices[i];
        let notional = self.qty * price;
        let fee = notional * self.cfg.maker_fee;
        let pnl = notional - fee - self.qty * cost_basis;

        self.quote += notional - fee;
        self.base -= self.qty;
        self.fees_paid += fee;
        self.realized_pnl += pnl;
        self.sell_fills += 1;

        self.slots[i] = Slot::Empty;
        self.slots[i - 1] = Slot::Buy;
        self.cursor = i;
        self.out_of_range = None;

        Event::Fill {
            side: Side::Sell,
            price,
            qty: self.qty,
            fee,
            realized_pnl: Some(pnl),
            counter_order: (Side::Buy, self.prices[i - 1]),
            step_profit: self.step_profit(i - 1),
            snapshot: self.snapshot(),
            stats: self.stats(),
        }
    }

    fn check_range(&mut self, tick: &Tick, events: &mut Vec<Event>) {
        let last = self.prices.len() - 1;
        let breakout_up =
            self.cursor == last && tick.bid > self.prices[last] * (1.0 + self.cfg.spacing);
        let breakout_down =
            self.cursor == 0 && tick.ask < self.prices[0] / (1.0 + self.cfg.spacing);

        if breakout_up && self.cfg.recenter_on_breakout {
            // Alle Sells sind gefüllt → wir halten nur Quote; Neuaufbau realisiert keinen Verlust.
            let old_low = self.prices[0];
            let old_high = self.prices[last];
            match self.place(tick, GridChange::Adjusted { old_low, old_high }) {
                Ok(event) => events.push(event),
                Err(e) => {
                    tracing::error!("Grid-Neuaufbau fehlgeschlagen: {e:#}");
                    self.cfg.recenter_on_breakout = false;
                }
            }
            return;
        }

        // Nach unten wird bewusst nicht nachgezogen (würde Verluste realisieren) – nur Alarm.
        let direction = match (breakout_up, breakout_down) {
            (true, _) => Direction::Above,
            (_, true) => Direction::Below,
            _ => return,
        };
        if self.out_of_range != Some(direction) {
            self.out_of_range = Some(direction);
            events.push(Event::OutOfRange {
                direction,
                price: tick.mid(),
                snapshot: self.snapshot(),
            });
        }
    }

    /// (Neu-)Aufbau des Grids um den aktuellen Mid-Preis.
    fn place(&mut self, tick: &Tick, change: GridChange) -> Result<Event> {
        let n = self.cfg.levels_per_side;
        let center = tick.mid();
        let (prices, qty) = build_levels(&self.cfg, center)?;

        // Kapitalbedarf: N Buy-Orders + Market-Kauf von N × qty für die Sell-Seite.
        let buy_side: f64 =
            prices[..n].iter().map(|p| p * qty).sum::<f64>() * (1.0 + self.cfg.maker_fee);
        let inventory_qty = qty * n as f64;
        let inventory_cost = inventory_qty * tick.ask;
        let inventory_fee = inventory_cost * self.cfg.taker_fee;
        let required = buy_side + inventory_cost + inventory_fee;
        // Bereits gehaltenes Basis-Asset (Rundungsreste) wird nicht angerechnet.
        if self.quote < required {
            bail!(
                "Kapital reicht nicht: benötigt {required:.2}, verfügbar {:.2} \
                 (ORDER_SIZE_QUOTE oder GRID_LEVELS_PER_SIDE senken)",
                self.quote
            );
        }

        self.quote -= inventory_cost + inventory_fee;
        self.base += inventory_qty;
        self.fees_paid += inventory_fee;

        let cost_basis = tick.ask * (1.0 + self.cfg.taker_fee);
        self.slots = (0..prices.len())
            .map(|i| match i.cmp(&n) {
                std::cmp::Ordering::Less => Slot::Buy,
                std::cmp::Ordering::Equal => Slot::Empty,
                std::cmp::Ordering::Greater => Slot::Sell { cost_basis },
            })
            .collect();
        self.prices = prices;
        self.qty = qty;
        self.cursor = n;
        self.mark_price = center;
        self.out_of_range = None;

        Ok(Event::Grid {
            change,
            center,
            inventory_buy: Some((inventory_qty, tick.ask, inventory_fee)),
            step_profit: self.step_profit(n),
            snapshot: self.snapshot(),
            stats: self.stats(),
        })
    }

    fn snapshot(&self) -> GridSnapshot {
        let lo = self.cursor.saturating_sub(SNAPSHOT_RADIUS);
        let hi = (self.cursor + SNAPSHOT_RADIUS).min(self.prices.len() - 1);
        let window = (lo..=hi)
            .rev()
            .map(|i| LevelView {
                price: self.prices[i],
                order: match self.slots[i] {
                    Slot::Empty => None,
                    Slot::Buy => Some(Side::Buy),
                    Slot::Sell { .. } => Some(Side::Sell),
                },
                is_cursor: i == self.cursor,
            })
            .collect();
        GridSnapshot {
            window,
            active_buys: self.slots.iter().filter(|s| **s == Slot::Buy).count(),
            active_sells: self
                .slots
                .iter()
                .filter(|s| matches!(s, Slot::Sell { .. }))
                .count(),
            low: self.prices[0],
            high: self.prices[self.prices.len() - 1],
            qty: self.qty,
            price_decimals: self.price_decimals,
            qty_decimals: self.qty_decimals,
        }
    }
}

/// Geometrisches Grid: `p_i = center · (1 + s)^(i - N)`, auf Tick-Size gerundet.
/// Prüft nach dem Runden für *jede* Stufe, dass der Netto-Gewinn nach Fees reicht.
fn build_levels(cfg: &GridConfig, center: f64) -> Result<(Vec<f64>, f64)> {
    let n = cfg.levels_per_side as i32;
    let prices: Vec<f64> = (-n..=n)
        .map(|k| round_to(center * (1.0 + cfg.spacing).powi(k), cfg.price_tick))
        .collect();

    let qty = floor_to(cfg.order_size_quote / center, cfg.qty_step);
    if qty <= 0.0 {
        bail!("ORDER_SIZE_QUOTE ist kleiner als ein QTY_STEP bei Preis {center:.4}");
    }
    if qty * prices[0] < cfg.min_notional {
        bail!(
            "Ordergröße am unteren Grid-Ende ({:.2}) liegt unter MIN_NOTIONAL ({:.2})",
            qty * prices[0],
            cfg.min_notional
        );
    }
    if prices[0] <= 0.0 {
        bail!("unterstes Grid-Level ≤ 0 – Spacing oder Level-Anzahl zu groß");
    }
    for w in prices.windows(2) {
        let step = net_step(w[0], w[1], qty, cfg.maker_fee);
        if step.pct < cfg.min_net_profit {
            bail!(
                "Grid-Stufe {:.4} → {:.4} bringt nach Fees nur {:.4} % (Minimum {:.4} %); \
                 GRID_SPACING_PCT erhöhen",
                w[0],
                w[1],
                step.pct * 100.0,
                cfg.min_net_profit * 100.0
            );
        }
    }
    Ok((prices, qty))
}

fn net_step(buy: f64, sell: f64, qty: f64, fee: f64) -> StepProfit {
    let cost = qty * buy * (1.0 + fee);
    let proceeds = qty * sell * (1.0 - fee);
    StepProfit {
        quote: proceeds - cost,
        pct: (proceeds - cost) / cost,
    }
}

fn round_to(x: f64, step: f64) -> f64 {
    (x / step).round() * step
}

fn floor_to(x: f64, step: f64) -> f64 {
    // Kleiner Epsilon-Zuschlag gegen f64-Artefakte wie 0.1/0.001 = 99.999…
    (x / step + 1e-9).floor() * step
}

fn decimals(step: f64) -> usize {
    (-step.log10()).ceil().max(0.0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn cfg() -> GridConfig {
        GridConfig {
            levels_per_side: 5,
            spacing: 0.005,
            order_size_quote: 20.0,
            maker_fee: 0.001,
            taker_fee: 0.001,
            min_net_profit: 0.0005,
            price_tick: 0.01,
            qty_step: 0.001,
            min_notional: 5.0,
            recenter_on_breakout: true,
        }
    }

    fn tick(bid: f64, ask: f64) -> Tick {
        Tick {
            bid,
            ask,
            received_at: Instant::now(),
        }
    }

    #[test]
    fn min_spacing_covers_fees() {
        let c = cfg();
        // (1.0005)(1.001)/(0.999) - 1 ≈ 0.2503 %
        assert!((c.min_spacing() - 0.002503).abs() < 1e-5);
    }

    #[test]
    fn rejects_spacing_below_fees() {
        let mut c = cfg();
        c.spacing = 0.0015;
        assert!(build_levels(&c, 150.0).is_err());
    }

    #[test]
    fn initial_layout_respects_invariant() {
        let (g, _) = Grid::new(cfg(), 1000.0, &tick(149.99, 150.01)).unwrap();
        assert_eq!(g.prices.len(), 11);
        assert_eq!(g.cursor, 5);
        assert!(g.slots[..5].iter().all(|s| *s == Slot::Buy));
        assert!(g.slots[6..].iter().all(|s| matches!(s, Slot::Sell { .. })));
        assert!((g.base - 5.0 * g.qty).abs() < 1e-9);
    }

    #[test]
    fn round_trip_is_net_positive() {
        let (mut g, _) = Grid::new(cfg(), 1000.0, &tick(149.99, 150.01)).unwrap();
        let start = g.stats().equity();
        let mut ev = Vec::new();

        let buy_px = g.prices[4];
        g.on_tick(&tick(buy_px - 0.02, buy_px), &mut ev);
        assert!(matches!(
            ev[0],
            Event::Fill {
                side: Side::Buy,
                ..
            }
        ));
        assert_eq!(g.cursor, 4);
        assert!(matches!(g.slots[5], Slot::Sell { .. }));

        ev.clear();
        let sell_px = g.prices[5];
        g.on_tick(&tick(sell_px, sell_px + 0.02), &mut ev);
        match &ev[0] {
            Event::Fill {
                side: Side::Sell,
                realized_pnl: Some(pnl),
                ..
            } => assert!(*pnl > 0.0, "pnl = {pnl}"),
            other => panic!("unerwartet: {other:?}"),
        }
        assert_eq!(g.cursor, 5);
        assert_eq!(g.slots[4], Slot::Buy);
        assert!(g.realized_pnl > 0.0);
        // Gleicher Preis wie am Start → Equity nur durch den Zyklus gestiegen.
        g.mark_price = 150.0;
        assert!(g.stats().equity() > start - 1e-9);
    }

    #[test]
    fn gap_fills_multiple_levels_in_one_tick() {
        let (mut g, _) = Grid::new(cfg(), 1000.0, &tick(149.99, 150.01)).unwrap();
        let mut ev = Vec::new();
        let px = g.prices[2];
        g.on_tick(&tick(px - 0.01, px), &mut ev);
        let buys = ev
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Event::Fill {
                        side: Side::Buy,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(buys, 3);
        assert_eq!(g.cursor, 2);
    }

    #[test]
    fn recenters_after_breakout_up() {
        let (mut g, _) = Grid::new(cfg(), 1000.0, &tick(149.99, 150.01)).unwrap();
        let mut ev = Vec::new();
        let top = g.prices[10];
        // Erst alle Sells füllen, dann deutlich darüber handeln.
        g.on_tick(&tick(top, top + 0.01), &mut ev);
        assert_eq!(g.cursor, 10);
        ev.clear();
        let far = top * 1.02;
        g.on_tick(&tick(far, far + 0.01), &mut ev);
        assert!(matches!(
            ev.last(),
            Some(Event::Grid {
                change: GridChange::Adjusted { .. },
                ..
            })
        ));
        assert_eq!(g.cursor, 5);
        assert!(g.prices[5] > top);
    }

    #[test]
    fn out_of_range_below_alerts_once() {
        let (mut g, _) = Grid::new(cfg(), 1000.0, &tick(149.99, 150.01)).unwrap();
        let mut ev = Vec::new();
        let low = g.prices[0] * 0.98;
        g.on_tick(&tick(low - 0.01, low), &mut ev);
        g.on_tick(&tick(low - 0.02, low - 0.01), &mut ev);
        let alerts = ev
            .iter()
            .filter(|e| matches!(e, Event::OutOfRange { .. }))
            .count();
        assert_eq!(alerts, 1);
    }

    #[test]
    fn insufficient_capital_is_rejected() {
        assert!(Grid::new(cfg(), 50.0, &tick(149.99, 150.01)).is_err());
    }
}
