# SOL/USDC Grid-Trading-Bot (Rust)

Asynchroner Low-Latency-Grid-Bot für **SOL/USDC** (Binance oder Bybit) mit
**Paper-Trading** über den Live-Websocket-Preisstream und **Discord-Alerts** für jede
Order-Platzierung, jeden Fill und jede Grid-Anpassung.

> ⚠️ Aktuell ist nur der **Dry-Run-Modus** implementiert. `DRY_RUN=false` wird beim
> Start bewusst abgelehnt, es werden also keine echten Orders gesendet.

## Architektur

```text
 [feed]  ──Tick (mpsc)──▶  [engine]  ──Event (mpsc)──▶  [discord]
 Websocket, Reconnect      Grid + Paper-Fills          Webhook, Batching, Rate-Limits
```

| Modul | Aufgabe |
|---|---|
| `src/main.rs` | Runtime, Task-Start (`tokio::spawn`), sauberer Shutdown (Ctrl-C/SIGTERM) |
| `src/config.rs` | `.env` laden, validieren, Mindest-Spacing nach Fees prüfen |
| `src/feed.rs` | Binance `bookTicker` / Bybit `orderbook.1`, Zero-Copy-Parsing, Reconnect mit Backoff, Daten-Timeout |
| `src/grid.rs` | Grid-Logik + Paper-Buchhaltung (synchron, unit-getestet) |
| `src/engine.rs` | Besitzt den Grid-State exklusiv (keine Locks), verarbeitet jeden Tick ohne I/O-`await` |
| `src/discord.rs` | Embeds bauen, bis zu 10 Embeds pro Nachricht bündeln, 429/`X-RateLimit-*` respektieren |

**Grid-Mechanik:** `2N + 1` geometrische Levels `p_i = mid · (1 + s)^(i − N)`. Unter dem
Preis liegen BUYs, darüber SELLs, das Level am Preis ist leer. Ein BUY-Fill erzeugt sofort
einen SELL eine Stufe höher, ein SELL-Fill einen BUY eine Stufe tiefer. Pro Tick wird
dadurch nur je eine Order pro Seite geprüft (O(1)). Für die Sell-Seite wird beim Start
das nötige SOL simuliert per Taker-Order gekauft.

**Fees:** Ein Zyklus BUY@p → SELL@p·(1+s) bringt netto
`q·p·(1+s)·(1−f) − q·p·(1+f)`. Beim Start wird für **jede** Stufe nach dem Runden auf
die Tick-Size geprüft, dass mindestens `MIN_NET_PROFIT_PCT` übrig bleibt. Sonst startet
der Bot nicht. Bei 0,1 % Fee und 0,05 % Mindestgewinn muss `GRID_SPACING_PCT` > 0,2503 % sein.

**Paper-Fills:** Ein BUY gilt als gefüllt, sobald `ask ≤ Limit`, ein SELL, sobald
`bid ≥ Limit`. Gefüllt wird zum Limitpreis mit Maker-Fee.

## Konfiguration (`.env`)

```bash
cp .env.example .env
nano .env        # DISCORD_WEBHOOK_URL eintragen, Grid-Parameter anpassen
chmod 600 .env   # Webhook-URL und API-Keys sind Geheimnisse
```

Beim Start lädt der Bot automatisch die `.env` aus dem aktuellen Arbeitsverzeichnis
(`dotenvy`). Bereits gesetzte Umgebungsvariablen haben Vorrang, z. B.
`GRID_SPACING_PCT=0.8 cargo run --release`. Alle Schlüssel mit Standardwerten stehen in
[`.env.example`](.env.example).

Discord-Webhook anlegen: *Servereinstellungen → Integrationen → Webhooks → Neuer Webhook →
Webhook-URL kopieren*.

## Ubuntu-VPS: Installation & Start

```bash
# 1. Build-Tools + Rust
sudo apt update && sudo apt install -y build-essential cmake pkg-config git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# 2. Code holen
git clone https://github.com/wunderschoenezitate-netizen/Tradingbot-on-chain.git
cd Tradingbot-on-chain
cp .env.example .env && nano .env

# 3. Tests + Release-Build (LTO, opt-level 3)
cargo test
cargo build --release

# 4. Starten
cargo run --release                     # oder direkt: ./target/release/sol-grid-bot
RUST_LOG=debug cargo run --release      # inkl. Tick→Fill-Latenz
```

TLS läuft über `rustls`, OpenSSL muss also nicht installiert werden.

### Dauerbetrieb mit systemd

```bash
sudo useradd --system --no-create-home gridbot
sudo mkdir -p /opt/sol-grid-bot
sudo cp target/release/sol-grid-bot .env /opt/sol-grid-bot/
sudo chown -R gridbot:gridbot /opt/sol-grid-bot && sudo chmod 600 /opt/sol-grid-bot/.env
sudo cp deploy/sol-grid-bot.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now sol-grid-bot
journalctl -u sol-grid-bot -f          # Live-Logs
sudo systemctl stop sol-grid-bot       # sendet Abschluss-Report an Discord
```

## Hinweise

- **Standort des VPS:** Binance.com und Bybit sperren u. a. US-IPs. Der VPS sollte in einer
  Region stehen, in der die Börse verfügbar ist, sonst schlägt der Websocket-Connect fehl.
- **Tick-/Lot-Size** (`PRICE_TICK`, `QTY_STEP`, `MIN_NOTIONAL`) mit den aktuellen
  Symbol-Regeln der Börse abgleichen.
- **Breakout nach unten** wird nur gemeldet, das Grid wird dann *nicht* nachgezogen, weil
  das Verluste realisieren würde. Nach oben (alles verkauft, nur USDC) wird es bei
  `RECENTER_ON_BREAKOUT=true` neu aufgebaut.
