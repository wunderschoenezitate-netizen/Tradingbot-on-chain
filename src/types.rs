//! Gemeinsame Datentypen, die zwischen den Tasks über Channels fließen.

use std::{fmt, time::Instant};

/// Bestes Bid/Ask aus dem Websocket-Feed.
#[derive(Debug, Clone, Copy)]
pub struct Tick {
    pub bid: f64,
    pub ask: f64,
    /// Zeitpunkt des Empfangs – zur Messung der Tick→Entscheidung-Latenz.
    pub received_at: Instant,
}

impl Tick {
    pub fn mid(&self) -> f64 {
        (self.bid + self.ask) * 0.5
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Above,
    Below,
}

/// Eine Zeile der Grid-Tabelle im Discord-Embed.
#[derive(Debug, Clone)]
pub struct LevelView {
    pub price: f64,
    pub order: Option<Side>,
    pub is_cursor: bool,
}

/// Momentaufnahme des Grids für Alerts.
#[derive(Debug, Clone)]
pub struct GridSnapshot {
    /// Ausschnitt um den aktuellen Preis, höchster Preis zuerst.
    pub window: Vec<LevelView>,
    pub active_buys: usize,
    pub active_sells: usize,
    pub low: f64,
    pub high: f64,
    pub qty: f64,
    pub price_decimals: usize,
    pub qty_decimals: usize,
}

/// Erwarteter Netto-Gewinn eines Buy→Sell-Zyklus zwischen zwei benachbarten Levels.
#[derive(Debug, Clone, Copy)]
pub struct StepProfit {
    pub quote: f64,
    pub pct: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Stats {
    pub realized_pnl: f64,
    pub fees_paid: f64,
    pub buy_fills: u64,
    pub sell_fills: u64,
    pub quote_balance: f64,
    pub base_balance: f64,
    pub mark_price: f64,
    pub start_equity: f64,
}

impl Stats {
    pub fn equity(&self) -> f64 {
        self.quote_balance + self.base_balance * self.mark_price
    }
}

#[derive(Debug, Clone)]
pub enum GridChange {
    Placed,
    Adjusted { old_low: f64, old_high: f64 },
}

/// Alles, was der Discord-Notifier meldet.
#[derive(Debug, Clone)]
pub enum Event {
    Grid {
        change: GridChange,
        center: f64,
        /// Market-Buy des Basis-Assets für die Sell-Seite: (Menge, Preis, Fee).
        inventory_buy: Option<(f64, f64, f64)>,
        step_profit: StepProfit,
        snapshot: GridSnapshot,
        stats: Stats,
    },
    Fill {
        side: Side,
        price: f64,
        qty: f64,
        fee: f64,
        /// Nur bei SELL: realisierter Netto-Gewinn dieses Grid-Zyklus.
        realized_pnl: Option<f64>,
        /// Sofort platzierte Gegenorder (Seite, Preis).
        counter_order: (Side, f64),
        step_profit: StepProfit,
        snapshot: GridSnapshot,
        stats: Stats,
    },
    OutOfRange {
        direction: Direction,
        price: f64,
        snapshot: GridSnapshot,
    },
    Shutdown {
        stats: Stats,
    },
}
