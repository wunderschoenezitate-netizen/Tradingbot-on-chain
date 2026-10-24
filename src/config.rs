//! Konfiguration aus Umgebungsvariablen bzw. `.env`.

use std::{env, fmt::Display, str::FromStr, time::Duration};

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exchange {
    Binance,
    Bybit,
}

impl FromStr for Exchange {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "binance" => Ok(Self::Binance),
            "bybit" => Ok(Self::Bybit),
            other => bail!("unbekannte Börse '{other}' (erlaubt: binance, bybit)"),
        }
    }
}

impl Display for Exchange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Binance => "Binance",
            Self::Bybit => "Bybit",
        })
    }
}

/// Parameter des Grids. Alle Prozentwerte sind intern als Bruch gespeichert (0.1 % → 0.001).
#[derive(Debug, Clone)]
pub struct GridConfig {
    pub levels_per_side: usize,
    pub spacing: f64,
    pub order_size_quote: f64,
    pub maker_fee: f64,
    pub taker_fee: f64,
    pub min_net_profit: f64,
    pub price_tick: f64,
    pub qty_step: f64,
    pub min_notional: f64,
    pub recenter_on_breakout: bool,
}

impl GridConfig {
    /// Kleinster Grid-Abstand, bei dem ein Buy→Sell-Zyklus nach Maker-Fees auf beiden
    /// Seiten noch `min_net_profit` abwirft:
    /// `(1 + s)(1 - f) / (1 + f) - 1 >= m  ⇔  s >= (1 + m)(1 + f) / (1 - f) - 1`
    pub fn min_spacing(&self) -> f64 {
        (1.0 + self.min_net_profit) * (1.0 + self.maker_fee) / (1.0 - self.maker_fee) - 1.0
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub exchange: Exchange,
    pub symbol: String,
    pub ws_url: String,
    pub dry_run: bool,
    pub discord_webhook_url: Option<String>,
    pub discord_username: String,
    /// Für einen späteren Live-Executor reserviert; im Dry-Run nicht benötigt.
    #[allow(dead_code)]
    pub api_key: Option<String>,
    #[allow(dead_code)]
    pub api_secret: Option<String>,
    pub paper_start_quote: f64,
    pub feed_timeout: Duration,
    pub grid: GridConfig,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let exchange: Exchange = var("EXCHANGE", Exchange::Binance)?;
        let symbol = var("SYMBOL", "SOLUSDC".to_string())?.to_ascii_uppercase();

        let ws_url = opt_var("WS_URL").unwrap_or_else(|| match exchange {
            Exchange::Binance => format!(
                "wss://stream.binance.com:9443/ws/{}@bookTicker",
                symbol.to_ascii_lowercase()
            ),
            Exchange::Bybit => "wss://stream.bybit.com/v5/public/spot".to_string(),
        });

        let pct = |key: &str, default: f64| -> Result<f64> { Ok(var(key, default)? / 100.0) };

        let grid = GridConfig {
            levels_per_side: var("GRID_LEVELS_PER_SIDE", 10usize)?,
            spacing: pct("GRID_SPACING_PCT", 0.5)?,
            order_size_quote: var("ORDER_SIZE_QUOTE", 25.0)?,
            maker_fee: pct("MAKER_FEE_PCT", 0.1)?,
            taker_fee: pct("TAKER_FEE_PCT", 0.1)?,
            min_net_profit: pct("MIN_NET_PROFIT_PCT", 0.05)?,
            price_tick: var("PRICE_TICK", 0.01)?,
            qty_step: var("QTY_STEP", 0.001)?,
            min_notional: var("MIN_NOTIONAL", 5.0)?,
            recenter_on_breakout: var("RECENTER_ON_BREAKOUT", true)?,
        };

        let cfg = Self {
            exchange,
            symbol,
            ws_url,
            dry_run: var("DRY_RUN", true)?,
            discord_webhook_url: opt_var("DISCORD_WEBHOOK_URL"),
            discord_username: var("DISCORD_USERNAME", "SOL Grid Bot".to_string())?,
            api_key: opt_var("API_KEY"),
            api_secret: opt_var("API_SECRET"),
            paper_start_quote: var("PAPER_START_QUOTE", 1000.0)?,
            feed_timeout: Duration::from_secs(var("FEED_TIMEOUT_SECS", 30u64)?),
            grid,
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let g = &self.grid;
        if !(1..=200).contains(&g.levels_per_side) {
            bail!("GRID_LEVELS_PER_SIDE muss zwischen 1 und 200 liegen");
        }
        for (name, v) in [
            ("ORDER_SIZE_QUOTE", g.order_size_quote),
            ("PRICE_TICK", g.price_tick),
            ("QTY_STEP", g.qty_step),
            ("PAPER_START_QUOTE", self.paper_start_quote),
        ] {
            if !(v.is_finite() && v > 0.0) {
                bail!("{name} muss > 0 sein (ist {v})");
            }
        }
        for (name, v) in [
            ("MAKER_FEE_PCT", g.maker_fee),
            ("TAKER_FEE_PCT", g.taker_fee),
        ] {
            if !(0.0..0.05).contains(&v) {
                bail!("{name} muss zwischen 0 und 5 % liegen");
            }
        }
        if g.min_net_profit < 0.0 {
            bail!("MIN_NET_PROFIT_PCT darf nicht negativ sein");
        }
        let min = g.min_spacing();
        if g.spacing < min {
            bail!(
                "GRID_SPACING_PCT={:.4} % ist zu klein: bei {:.3} % Maker-Fee und {:.3} % \
                 Mindestgewinn sind mindestens {:.4} % nötig",
                g.spacing * 100.0,
                g.maker_fee * 100.0,
                g.min_net_profit * 100.0,
                min * 100.0
            );
        }
        if let Some(url) = &self.discord_webhook_url {
            if !url.starts_with("https://discord.com/api/webhooks/")
                && !url.starts_with("https://discordapp.com/api/webhooks/")
            {
                tracing::warn!("DISCORD_WEBHOOK_URL sieht nicht wie eine Discord-Webhook-URL aus");
            }
        }
        Ok(())
    }
}

/// Liest `key`, fällt auf `default` zurück, wenn die Variable fehlt oder leer ist.
fn var<T>(key: &str, default: T) -> Result<T>
where
    T: FromStr,
    T::Err: Display,
{
    match opt_var(key) {
        None => Ok(default),
        Some(raw) => raw
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("{e}"))
            .with_context(|| format!("ungültiger Wert für {key}: '{raw}'")),
    }
}

fn opt_var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
