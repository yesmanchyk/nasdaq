//! Standalone Real-Time ITCH 5.0 Market Data & OUCH Order Entry GUI Client (`itch_client`)
//!
//! Uses `eframe` / `egui` + `egui_plot` to render:
//! - Real-Time Streaming Price Chart (Last Trade, Mid Price, Best Bid / Best Ask envelope, Trade Dots)
//! - Level-2 Market Depth Order Book Ladder with volume bars & interactive price pick
//! - Interactive OUCH 5.0 Order Entry (Buy/Sell Limit Orders, Price & Quantity Steppers, Quick Presets)
//! - Active Order Blotter with per-order Cancel and Cancel All buttons, Real-Time Position & MTM PnL
//! - Live Time & Sales (Public Execution Tape) and PGM Multicast Reliability Telemetry
//! - 1-Click Built-in Simulation Launcher (starts Exchange + 2 Bots and auto-connects OUCH)
//!
//! Usage:
//!   cargo run --release --bin itch_client
//!   cargo run --release --bin itch_client -- --ouch 127.0.0.1:40000
//!   cargo run --release --bin itch_client -- --cli --duration-ms 2500

use eframe::egui::{self, Color32, CornerRadius, FontId, RichText, Stroke, Vec2};
use egui_plot::{Legend, Line, Plot, PlotPoints, Points};
use itch_ouch_sim::bot::{BotState, ShadowBook, default_strategies_for_bot, spawn_bot};
use itch_ouch_sim::exchange::ExchangeServer;
use itch_ouch_sim::pgm::{
    DEFAULT_MCAST_GROUP, DEFAULT_MCAST_PORT, DEFAULT_NAK_ADDR, DEFAULT_OUCH_ADDR, PgmSubscriber,
};
use itch_ouch_sim::protocol::{
    ItchMessage, OuchInbound, OuchOutbound, PRICE_SCALE, Side, fmt_price, try_read_frame,
    write_frame,
};
use std::collections::VecDeque;
use std::net::{Ipv4Addr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

const MAX_CHART_POINTS: usize = 4_000;
const MAX_TAPE_TRADES: usize = 200;

#[derive(Clone, Copy)]
struct TradeRecord {
    time_s: f64,
    seq_num: u64,
    order_ref: u64,
    side: Side,
    shares: u32,
    price_fp: u64,
}

#[derive(Clone, Copy)]
struct PriceSample {
    time_s: f64,
    last_px: Option<f64>,
    mid_px: f64,
    bid_px: Option<f64>,
    ask_px: Option<f64>,
}

struct SharedMarketState {
    book: ShadowBook,
    start_instant: Instant,
    total_itch_msgs: u64,
    total_trades: u64,
    total_volume: u64,
    prev_trade_price: u64,
    last_tick_dir: i8, // 1 = uptick, -1 = downtick, 0 = flat
    naks_sent: u64,
    rdata_recv: u64,
    samples: VecDeque<PriceSample>,
    trades: VecDeque<TradeRecord>,
}

impl SharedMarketState {
    fn new() -> Self {
        Self {
            book: ShadowBook::default(),
            start_instant: Instant::now(),
            total_itch_msgs: 0,
            total_trades: 0,
            total_volume: 0,
            prev_trade_price: 0,
            last_tick_dir: 0,
            naks_sent: 0,
            rdata_recv: 0,
            samples: VecDeque::with_capacity(MAX_CHART_POINTS),
            trades: VecDeque::with_capacity(MAX_TAPE_TRADES),
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn on_itch(&mut self, msg: ItchMessage, naks_sent: u64, rdata_recv: u64) {
        let elapsed_s = self.start_instant.elapsed().as_secs_f64();
        self.total_itch_msgs += 1;
        self.naks_sent = naks_sent;
        self.rdata_recv = rdata_recv;

        if let ItchMessage::OrderExecuted {
            seq_num,
            order_ref,
            side,
            executed_shares,
            match_price,
        } = msg
        {
            if self.book.last_trade_price > 0 {
                if match_price > self.book.last_trade_price {
                    self.last_tick_dir = 1;
                } else if match_price < self.book.last_trade_price {
                    self.last_tick_dir = -1;
                }
                self.prev_trade_price = self.book.last_trade_price;
            }
            self.total_trades += 1;
            self.total_volume += executed_shares as u64;

            if self.trades.len() >= MAX_TAPE_TRADES {
                self.trades.pop_back();
            }
            self.trades.push_front(TradeRecord {
                time_s: elapsed_s,
                seq_num,
                order_ref,
                side,
                shares: executed_shares,
                price_fp: match_price,
            });
        }

        self.book.apply(msg);

        let last_px = if self.book.last_trade_price > 0 {
            Some(self.book.last_trade_price as f64 / PRICE_SCALE as f64)
        } else {
            None
        };
        let mid_px = self.book.mid_price() as f64 / PRICE_SCALE as f64;
        let bid_px = self
            .book
            .best_bid()
            .map(|(p, _)| p as f64 / PRICE_SCALE as f64);
        let ask_px = self
            .book
            .best_ask()
            .map(|(p, _)| p as f64 / PRICE_SCALE as f64);

        let should_sample = matches!(msg, ItchMessage::OrderExecuted { .. })
            || self
                .samples
                .back()
                .is_none_or(|last| (elapsed_s - last.time_s) >= 0.005);

        if should_sample {
            if self.samples.len() >= MAX_CHART_POINTS {
                self.samples.pop_front();
            }
            self.samples.push_back(PriceSample {
                time_s: elapsed_s,
                last_px,
                mid_px,
                bid_px,
                ask_px,
            });
        }
    }
}

// ============================================================================
// OUCH Client (TCP Order Entry & Execution Management)
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OrderStatus {
    PendingNew,
    Open,
    PartiallyFilled,
    Filled,
    Canceled,
}

#[derive(Clone, Debug)]
struct UserOrder {
    cl_ord_id: u64,
    order_ref: Option<u64>,
    side: Side,
    price_fp: u64,
    initial_shares: u32,
    remaining_shares: u32,
    status: OrderStatus,
    timestamp_s: f64,
}

struct OuchClient {
    cmd_tx: Option<mpsc::Sender<OuchInbound>>,
    event_rx: Option<mpsc::Receiver<OuchOutbound>>,
    is_connected: Arc<AtomicBool>,
    connected_addr: Arc<Mutex<String>>,
    worker_stop: Option<Arc<AtomicBool>>,
}

impl OuchClient {
    fn new() -> Self {
        Self {
            cmd_tx: None,
            event_rx: None,
            is_connected: Arc::new(AtomicBool::new(false)),
            connected_addr: Arc::new(Mutex::new(String::new())),
            worker_stop: None,
        }
    }

    fn connect(&mut self, addr: &str) -> Result<(), String> {
        if let Some(stop) = self.worker_stop.take() {
            stop.store(false, Ordering::Relaxed);
        }
        self.is_connected.store(false, Ordering::Relaxed);

        let stream = TcpStream::connect(addr).map_err(|e| format!("{e}"))?;
        stream.set_nodelay(true).map_err(|e| format!("{e}"))?;
        let mut reader = stream.try_clone().map_err(|e| format!("{e}"))?;
        reader.set_nonblocking(true).map_err(|e| format!("{e}"))?;
        let mut writer = stream;

        let (cmd_tx, cmd_rx) = mpsc::channel::<OuchInbound>();
        let (event_tx, event_rx) = mpsc::channel::<OuchOutbound>();

        let is_conn = Arc::clone(&self.is_connected);
        is_conn.store(true, Ordering::Relaxed);
        let stop_flag = Arc::new(AtomicBool::new(true));
        let worker_stop = Arc::clone(&stop_flag);

        *self.connected_addr.lock().unwrap() = addr.to_string();

        // Writer Thread
        let is_conn_w = Arc::clone(&is_conn);
        let stop_w = Arc::clone(&stop_flag);
        thread::Builder::new()
            .name("ouch-gui-writer".into())
            .spawn(move || {
                while stop_w.load(Ordering::Relaxed) && is_conn_w.load(Ordering::Relaxed) {
                    match cmd_rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(cmd) => {
                            let bytes = cmd.to_bytes();
                            if write_frame(&mut writer, &bytes).is_err() {
                                is_conn_w.store(false, Ordering::Relaxed);
                                break;
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .ok();

        // Reader Thread
        let is_conn_r = Arc::clone(&is_conn);
        let stop_r = Arc::clone(&stop_flag);
        thread::Builder::new()
            .name("ouch-gui-reader".into())
            .spawn(move || {
                while stop_r.load(Ordering::Relaxed) && is_conn_r.load(Ordering::Relaxed) {
                    match try_read_frame::<{ OuchOutbound::WIRE_SIZE }>(&mut reader) {
                        Ok(Some(raw)) => {
                            if let Some(msg) = OuchOutbound::from_bytes(&raw) {
                                if event_tx.send(msg).is_err() {
                                    break;
                                }
                            }
                        }
                        Ok(None) => thread::sleep(Duration::from_millis(1)),
                        Err(_) => {
                            is_conn_r.store(false, Ordering::Relaxed);
                            break;
                        }
                    }
                }
            })
            .ok();

        self.cmd_tx = Some(cmd_tx);
        self.event_rx = Some(event_rx);
        self.worker_stop = Some(worker_stop);

        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.is_connected.load(Ordering::Relaxed)
    }

    fn send(&self, cmd: OuchInbound) -> bool {
        if let Some(ref tx) = self.cmd_tx {
            tx.send(cmd).is_ok()
        } else {
            false
        }
    }
}

impl Drop for OuchClient {
    fn drop(&mut self) {
        if let Some(stop) = self.worker_stop.take() {
            stop.store(false, Ordering::Relaxed);
        }
        self.is_connected.store(false, Ordering::Relaxed);
    }
}

fn parse_price_fp(s: &str) -> Option<u64> {
    let s = s.trim().trim_start_matches('$');
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.len() > 2 {
        return None;
    }
    let dollars: u64 = parts[0].parse().ok()?;
    let cents_str = if parts.len() == 2 { parts[1] } else { "" };
    if cents_str.len() > 4 {
        let truncated = &cents_str[..4];
        let frac: u64 = truncated.parse().ok()?;
        Some(dollars * PRICE_SCALE + frac)
    } else {
        let padded = format!("{:0<4}", cents_str);
        let frac: u64 = padded.parse().ok()?;
        Some(dollars * PRICE_SCALE + frac)
    }
}

// ============================================================================
// GUI Application State & Widgets
// ============================================================================

#[derive(PartialEq, Eq)]
enum BottomTab {
    OrdersAndPosition,
    TimeAndSales,
    BotPnl,
}

/// A built-in simulation bot tracked by the GUI for live PnL display
struct SimBot {
    state: Arc<BotState>,
    /// (elapsed seconds since sim start, MTM PnL in dollars)
    pnl_history: Vec<[f64; 2]>,
}

const BOT_COLORS: [Color32; 4] = [
    Color32::from_rgb(80, 190, 250),
    Color32::from_rgb(245, 190, 60),
    Color32::from_rgb(200, 120, 255),
    Color32::from_rgb(50, 225, 150),
];

struct ItchGuiApp {
    state: Arc<Mutex<SharedMarketState>>,
    mcast_group: Ipv4Addr,
    mcast_port: u16,
    nak_addr: String,
    ouch_addr: String,
    ouch_client: OuchClient,

    // Order Entry Controls
    order_side: Side,
    order_price_str: String,
    order_shares: u32,
    next_cl_ord_id: u64,
    order_status_feedback: Option<(String, bool)>, // (msg, is_error)

    // User Orders & Position
    user_orders: VecDeque<UserOrder>,
    user_position: i64,
    user_cash_fp: i64,
    user_fills: u64,
    user_cancels: u64,

    // UI Configuration
    depth_levels: usize,
    chart_window_secs: f64,
    show_last_price: bool,
    show_mid_price: bool,
    show_bid_ask: bool,
    show_trade_markers: bool,
    paused: bool,
    bottom_tab: BottomTab,
    demo_sim_running: Option<Arc<AtomicBool>>,
    status_msg: String,

    // Built-in simulation bots (kept after stop so final PnL stays visible)
    sim_bots: Vec<SimBot>,
    sim_started_at: Option<Instant>,
    sim_stopped_after_s: Option<f64>,
}

impl ItchGuiApp {
    fn new(
        state: Arc<Mutex<SharedMarketState>>,
        mcast_group: Ipv4Addr,
        mcast_port: u16,
        nak_addr: String,
        ouch_addr: String,
    ) -> Self {
        let mut app = Self {
            state,
            mcast_group,
            mcast_port,
            nak_addr,
            ouch_addr: ouch_addr.clone(),
            ouch_client: OuchClient::new(),
            order_side: Side::Buy,
            order_price_str: "100.0000".to_string(),
            order_shares: 100,
            next_cl_ord_id: 9_000_001,
            order_status_feedback: None,
            user_orders: VecDeque::new(),
            user_position: 0,
            user_cash_fp: 0,
            user_fills: 0,
            user_cancels: 0,
            depth_levels: 10,
            chart_window_secs: 15.0,
            show_last_price: true,
            show_mid_price: true,
            show_bid_ask: true,
            show_trade_markers: true,
            paused: false,
            bottom_tab: BottomTab::OrdersAndPosition,
            demo_sim_running: None,
            status_msg: "Listening on PGM Multicast".to_string(),
            sim_bots: Vec::new(),
            sim_started_at: None,
            sim_stopped_after_s: None,
        };

        if app.ouch_client.connect(&ouch_addr).is_ok() {
            app.status_msg = format!("Connected to OUCH {ouch_addr}");
        }

        app
    }

    fn toggle_builtin_sim(&mut self) {
        if let Some(flag) = self.demo_sim_running.take() {
            flag.store(false, Ordering::Relaxed);
            // Take one last PnL sample so the final result is on the chart
            self.sample_bot_pnl(true);
            let stopped_s = self.sim_started_at.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
            self.sim_stopped_after_s = Some(stopped_s);

            let perf_records: Vec<itch_ouch_sim::bot::BotRunPerformance> = self
                .sim_bots
                .iter()
                .enumerate()
                .map(|(idx, b)| itch_ouch_sim::bot::BotRunPerformance {
                    bot_id: (idx + 1) as u64,
                    name: b.state.name.clone(),
                    pnl: b.state.pnl_snapshot(),
                })
                .collect();
            let duration_ms = (stopped_s * 1000.0).round() as u64;
            let hist_suffix = match itch_ouch_sim::bot::append_run_history("gui_builtin", duration_ms, &perf_records) {
                Ok(p) => format!(" — saved to {}", p.display()),
                Err(_) => String::new(),
            };

            self.status_msg = format!("Stopped built-in simulation — final bot PnL in 🤖 BOT PnL tab{hist_suffix}");
            self.bottom_tab = BottomTab::BotPnl;
            return;
        }

        let mcast_group = self.mcast_group;
        let mcast_port = self.mcast_port;
        let nak_addr = self.nak_addr.clone();
        let running = Arc::new(AtomicBool::new(true));
        let sim_flag = Arc::clone(&running);

        match ExchangeServer::bind("127.0.0.1:0", mcast_group, mcast_port, &nak_addr) {
            Ok(mut exchange) => {
                let ouch_addr = match exchange.local_ouch_addr() {
                    Ok(a) => a.to_string(),
                    Err(e) => {
                        self.status_msg = format!("Failed to read OUCH addr: {e}");
                        return;
                    }
                };
                let ex_run = Arc::clone(&running);
                thread::Builder::new()
                    .name("gui-exchange".into())
                    .spawn(move || {
                        exchange.run(ex_run);
                    })
                    .ok();

                self.sim_bots.clear();
                for bot_id in 1..=4u64 {
                    let (name, strats) = default_strategies_for_bot(bot_id);
                    match spawn_bot(
                        name,
                        bot_id,
                        &ouch_addr,
                        mcast_group,
                        mcast_port,
                        &nak_addr,
                        strats,
                        Arc::clone(&running),
                    ) {
                        // Threads are detached; we only keep the shared state for PnL display
                        Ok(handle) => self.sim_bots.push(SimBot {
                            state: Arc::clone(&handle.state),
                            pnl_history: Vec::new(),
                        }),
                        Err(e) => {
                            self.status_msg = format!("Failed to start bot {bot_id}: {e}");
                        }
                    }
                }
                self.sim_started_at = Some(Instant::now());
                self.sim_stopped_after_s = None;

                self.demo_sim_running = Some(sim_flag);
                self.ouch_addr = ouch_addr.clone();
                let _ = self.ouch_client.connect(&ouch_addr);
                self.status_msg = format!(
                    "Built-in Exchange + {} Bots running on {ouch_addr}",
                    self.sim_bots.len()
                );
            }
            Err(e) => {
                self.status_msg = format!("Exchange port busy ({e}) — listening passively");
            }
        }
    }

    /// Append a PnL sample for each built-in bot (throttled to ~10 Hz unless forced).
    fn sample_bot_pnl(&mut self, force: bool) {
        let Some(start) = self.sim_started_at else {
            return;
        };
        if self.demo_sim_running.is_none() && !force {
            return;
        }
        let t = start.elapsed().as_secs_f64();
        for bot in &mut self.sim_bots {
            let due = bot
                .pnl_history
                .last()
                .is_none_or(|last| t - last[0] >= 0.1);
            if force || due {
                let pnl = bot.state.pnl_snapshot().mtm_pnl_fp as f64 / PRICE_SCALE as f64;
                bot.pnl_history.push([t, pnl]);
            }
        }
    }

    fn render_bot_pnl_tab(&self, ui: &mut egui::Ui, green: Color32, red: Color32, muted: Color32) {
        if self.sim_bots.is_empty() {
            ui.add_space(12.0);
            ui.label(
                RichText::new(
                    "No built-in bots yet. Click ▶ Start Built-in Sim to run the Exchange + 4 Bots \
                     and track how much money each bot makes.",
                )
                .color(muted),
            );
            ui.label(
                RichText::new(
                    "(Bots launched as separate `bot` processes run in their own address space \
                     and print their PnL to their own terminal.)",
                )
                .small()
                .color(muted),
            );
            return;
        }

        let elapsed = match (self.sim_stopped_after_s, self.sim_started_at) {
            (Some(s), _) => s,
            (None, Some(t)) => t.elapsed().as_secs_f64(),
            _ => 0.0,
        };
        let status = if self.demo_sim_running.is_some() {
            RichText::new(format!("● LIVE  t = {elapsed:.1}s")).color(green).strong()
        } else {
            RichText::new(format!("■ FINAL RESULTS  (ran {elapsed:.1}s)"))
                .color(Color32::WHITE)
                .strong()
        };

        // Rank bots by PnL (best first)
        let mut rows: Vec<(usize, &SimBot, itch_ouch_sim::bot::BotPnl)> = self
            .sim_bots
            .iter()
            .enumerate()
            .map(|(i, b)| (i, b, b.state.pnl_snapshot()))
            .collect();
        rows.sort_by(|a, b| b.2.mtm_pnl_fp.cmp(&a.2.mtm_pnl_fp));
        let total_fp: i64 = rows.iter().map(|r| r.2.mtm_pnl_fp).sum();

        let init_cap_dollars = rows
            .first()
            .map(|r| r.2.initial_capital_fp as f64 / PRICE_SCALE as f64)
            .unwrap_or(itch_ouch_sim::bot::INITIAL_BOT_CAPITAL_DOLLARS as f64);

        ui.columns(2, |cols| {
            // ---- Left: Leaderboard table ----
            let ui = &mut cols[0];
            ui.horizontal(|ui| {
                ui.label(status);
                ui.label(
                    RichText::new(format!(
                        "| Equal Starting Capital: ${init_cap_dollars:.2} each | Net Bot PnL: ${:+.2}",
                        total_fp as f64 / PRICE_SCALE as f64
                    ))
                    .small()
                    .color(muted),
                );
            });
            ui.add_space(4.0);
            egui::Grid::new("bot_pnl_grid")
                .striped(true)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for h in [
                        "#",
                        "BOT",
                        "START CAP",
                        "EQUITY",
                        "MTM PnL",
                        "CASH",
                        "POSITION",
                        "FILLS",
                        "ORDERS",
                        "RL STEPS",
                    ] {
                        ui.label(RichText::new(h).small().color(muted));
                    }
                    ui.end_row();

                    for (rank, (idx, bot, p)) in rows.iter().enumerate() {
                        let color = BOT_COLORS[*idx % BOT_COLORS.len()];
                        let start_cap = p.initial_capital_fp as f64 / PRICE_SCALE as f64;
                        let equity = p.equity_fp as f64 / PRICE_SCALE as f64;
                        let pnl = p.mtm_pnl_fp as f64 / PRICE_SCALE as f64;
                        let pnl_col = if pnl > 0.0 {
                            green
                        } else if pnl < 0.0 {
                            red
                        } else {
                            Color32::WHITE
                        };
                        let medal = match rank {
                            0 => "🥇",
                            1 => "🥈",
                            2 => "🥉",
                            _ => "",
                        };
                        ui.label(RichText::new(format!("{medal}{}", rank + 1)).strong());
                        ui.label(RichText::new(bot.state.name.trim()).strong().color(color));
                        ui.label(
                            RichText::new(format!("${start_cap:.2}"))
                                .monospace()
                                .color(muted),
                        );
                        ui.label(
                            RichText::new(format!("${equity:.2}"))
                                .monospace()
                                .strong()
                                .color(Color32::WHITE),
                        );
                        ui.label(
                            RichText::new(format!("${pnl:+.2}"))
                                .monospace()
                                .strong()
                                .color(pnl_col),
                        );
                        ui.label(
                            RichText::new(format!(
                                "${:.2}",
                                p.cash_fp as f64 / PRICE_SCALE as f64
                            ))
                            .monospace()
                            .color(muted),
                        );
                        ui.label(RichText::new(format!("{:+} sh", p.position)).monospace());
                        ui.label(RichText::new(format!("{}", p.fills)).monospace());
                        ui.label(
                            RichText::new(format!("{} / {}x", p.acks, p.cancels))
                                .monospace()
                                .color(muted),
                        );
                        let rl_label = if p.rl_q_updates > 0 || p.rl_bandit_updates > 0 {
                            format!("Q:{} B:{}", p.rl_q_updates, p.rl_bandit_updates)
                        } else {
                            "—".to_string()
                        };
                        ui.label(RichText::new(rl_label).monospace().color(muted));
                        ui.end_row();
                    }
                });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Each bot starts with an equal, fixed $100,000.00 capital budget. Equity = Cash + Position × Mid; MTM PnL = Equity − Start Cap.",
                )
                .small()
                .color(muted),
            );

            // ---- Right: PnL over time chart ----
            let ui = &mut cols[1];
            Plot::new("bot_pnl_plot")
                .legend(Legend::default())
                .allow_scroll(false)
                .y_axis_label("PnL ($)")
                .x_axis_label("sim time (s)")
                .show(ui, |plot_ui| {
                    for (i, bot) in self.sim_bots.iter().enumerate() {
                        if bot.pnl_history.is_empty() {
                            continue;
                        }
                        plot_ui.line(
                            Line::new(PlotPoints::new(bot.pnl_history.clone()))
                                .name(bot.state.name.trim())
                                .color(BOT_COLORS[i % BOT_COLORS.len()])
                                .width(2.0_f32),
                        );
                    }
                });
        });
    }

    fn submit_order(&mut self) {
        if !self.ouch_client.is_connected() {
            self.order_status_feedback = Some((
                "Cannot submit: OUCH disconnected. Connect to exchange first.".to_string(),
                true,
            ));
            return;
        }

        let Some(price_fp) = parse_price_fp(&self.order_price_str) else {
            self.order_status_feedback = Some((
                format!("Invalid price '{}'. Example: 100.25", self.order_price_str),
                true,
            ));
            return;
        };

        if price_fp == 0 {
            self.order_status_feedback =
                Some(("Order price must be greater than $0.0000".to_string(), true));
            return;
        }

        if self.order_shares == 0 {
            self.order_status_feedback =
                Some(("Shares must be greater than 0".to_string(), true));
            return;
        }

        let cl_ord_id = self.next_cl_ord_id;
        self.next_cl_ord_id += 1;

        let now_s = self
            .state
            .lock()
            .unwrap()
            .start_instant
            .elapsed()
            .as_secs_f64();

        self.user_orders.push_front(UserOrder {
            cl_ord_id,
            order_ref: None,
            side: self.order_side,
            price_fp,
            initial_shares: self.order_shares,
            remaining_shares: self.order_shares,
            status: OrderStatus::PendingNew,
            timestamp_s: now_s,
        });

        let cmd = OuchInbound::EnterOrder {
            cl_ord_id,
            side: self.order_side,
            shares: self.order_shares,
            price: price_fp,
        };

        if self.ouch_client.send(cmd) {
            let side_name = match self.order_side {
                Side::Buy => "BUY",
                Side::Sell => "SELL",
            };
            self.order_status_feedback = Some((
                format!(
                    "Submitted #{cl_ord_id}: {side_name} {} sh @ ${}",
                    self.order_shares,
                    fmt_price(price_fp)
                ),
                false,
            ));
        } else {
            self.order_status_feedback =
                Some(("Failed to send order down OUCH TCP socket".to_string(), true));
        }
    }

    fn cancel_order(&mut self, cl_ord_id: u64) {
        if !self.ouch_client.is_connected() {
            self.order_status_feedback =
                Some(("Cannot cancel: OUCH is disconnected".to_string(), true));
            return;
        }

        let cmd = OuchInbound::CancelOrder { cl_ord_id };
        if self.ouch_client.send(cmd) {
            self.order_status_feedback =
                Some((format!("Cancel request sent for #{cl_ord_id}"), false));
        }
    }

    fn cancel_all_active_orders(&mut self) {
        if !self.ouch_client.is_connected() {
            return;
        }
        let mut count = 0;
        for o in &self.user_orders {
            if matches!(
                o.status,
                OrderStatus::PendingNew | OrderStatus::Open | OrderStatus::PartiallyFilled
            ) {
                let _ = self.ouch_client.send(OuchInbound::CancelOrder {
                    cl_ord_id: o.cl_ord_id,
                });
                count += 1;
            }
        }
        self.order_status_feedback =
            Some((format!("Sent cancel requests for {count} open orders"), false));
    }

    fn process_ouch_events(&mut self) {
        if let Some(ref rx) = self.ouch_client.event_rx {
            while let Ok(event) = rx.try_recv() {
                match event {
                    OuchOutbound::OrderAccepted {
                        cl_ord_id,
                        order_ref,
                        side: _,
                        shares,
                        price: _,
                    } => {
                        if let Some(ord) = self
                            .user_orders
                            .iter_mut()
                            .find(|o| o.cl_ord_id == cl_ord_id)
                        {
                            ord.order_ref = Some(order_ref);
                            ord.remaining_shares = shares;
                            ord.status = OrderStatus::Open;
                        }
                    }
                    OuchOutbound::OrderExecuted {
                        cl_ord_id,
                        order_ref,
                        side,
                        executed_shares,
                        fill_price,
                    } => {
                        let mut is_mine = false;
                        if let Some(ord) = self
                            .user_orders
                            .iter_mut()
                            .find(|o| o.cl_ord_id == cl_ord_id)
                        {
                            is_mine = true;
                            ord.order_ref = Some(order_ref);
                            ord.remaining_shares =
                                ord.remaining_shares.saturating_sub(executed_shares);
                            if ord.remaining_shares == 0 {
                                ord.status = OrderStatus::Filled;
                            } else {
                                ord.status = OrderStatus::PartiallyFilled;
                            }
                        }

                        if is_mine {
                            self.user_fills += 1;
                            let qty = executed_shares as i64;
                            let notional = qty * (fill_price as i64);
                            match side {
                                Side::Buy => {
                                    self.user_position += qty;
                                    self.user_cash_fp -= notional;
                                }
                                Side::Sell => {
                                    self.user_position -= qty;
                                    self.user_cash_fp += notional;
                                }
                            }
                        }
                    }
                    OuchOutbound::OrderCanceled {
                        cl_ord_id,
                        order_ref: _,
                        canceled_shares: _,
                    } => {
                        if let Some(ord) = self
                            .user_orders
                            .iter_mut()
                            .find(|o| o.cl_ord_id == cl_ord_id)
                        {
                            ord.status = OrderStatus::Canceled;
                            self.user_cancels += 1;
                        }
                    }
                }
            }
        }
    }
}

impl Drop for ItchGuiApp {
    fn drop(&mut self) {
        if let Some(flag) = self.demo_sim_running.take() {
            flag.store(false, Ordering::Relaxed);
        }
    }
}

impl eframe::App for ItchGuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.paused {
            ctx.request_repaint_after(Duration::from_millis(33));
        }

        // 1. Drain OUCH Execution Reports & Acks
        self.process_ouch_events();
        self.sample_bot_pnl(false);

        // 2. Snapshot Market Feed under lock
        let (
            best_bid,
            best_ask,
            mid_fp,
            last_fp,
            spread_fp,
            tick_dir,
            total_itch,
            total_trades,
            total_vol,
            naks,
            rdata,
            top_asks,
            top_bids,
            total_bid_sh,
            total_ask_sh,
            samples,
            recent_trades,
        ) = {
            let st = self.state.lock().unwrap();
            (
                st.book.best_bid(),
                st.book.best_ask(),
                st.book.mid_price(),
                st.book.last_trade_price,
                st.book.spread(),
                st.last_tick_dir,
                st.total_itch_msgs,
                st.total_trades,
                st.total_volume,
                st.naks_sent,
                st.rdata_recv,
                st.book.top_asks(self.depth_levels),
                st.book.top_bids(self.depth_levels),
                st.book.total_bid_shares(),
                st.book.total_ask_shares(),
                st.samples.iter().copied().collect::<Vec<_>>(),
                st.trades.iter().take(60).copied().collect::<Vec<_>>(),
            )
        };

        let green = Color32::from_rgb(0, 210, 120);
        let red = Color32::from_rgb(245, 80, 95);
        let cyan = Color32::from_rgb(80, 190, 250);
        let amber = Color32::from_rgb(245, 190, 60);
        let muted = Color32::from_rgb(150, 160, 175);

        // ====================================================================
        // TOP PANEL: Controls + Headline Market Telemetry
        // ====================================================================
        egui::TopBottomPanel::top("top_header")
            .frame(
                egui::Frame::side_top_panel(&ctx.style())
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.heading(
                        RichText::new("NASDAQ ITCH 5.0 TERMINAL")
                            .strong()
                            .color(Color32::WHITE),
                    );
                    ui.separator();
                    ui.label(
                        RichText::new(format!(
                            "PGM {}:{} | NAK {}",
                            self.mcast_group, self.mcast_port, self.nak_addr
                        ))
                        .small()
                        .color(muted),
                    );
                    ui.separator();

                    let sim_btn_label = if self.demo_sim_running.is_some() {
                        RichText::new("⏹ Stop Built-in Sim").color(red).strong()
                    } else {
                        RichText::new("▶ Start Built-in Sim (Exchange + 4 Bots)")
                            .color(green)
                            .strong()
                    };
                    if ui.button(sim_btn_label).clicked() {
                        self.toggle_builtin_sim();
                    }

                    if ui
                        .button(if self.paused { "▶ Resume UI" } else { "⏸ Pause UI" })
                        .clicked()
                    {
                        self.paused = !self.paused;
                    }

                    if ui.button("🗑 Reset Market Data").clicked() {
                        self.state.lock().unwrap().reset();
                    }

                    ui.separator();
                    ui.label(RichText::new(&self.status_msg).small().color(cyan));
                });

                ui.add_space(6.0);

                // KPI Cards Row
                ui.columns(6, |cols| {
                    let last_color = match tick_dir {
                        1 => green,
                        -1 => red,
                        _ => Color32::WHITE,
                    };
                    let arrow = match tick_dir {
                        1 => " ▲",
                        -1 => " ▼",
                        _ => "",
                    };
                    let last_txt = if last_fp > 0 {
                        format!("${}{}", fmt_price(last_fp), arrow)
                    } else {
                        "N/A".to_string()
                    };
                    render_kpi_card(&mut cols[0], "LAST TRADE PRICE", &last_txt, last_color);

                    render_kpi_card(
                        &mut cols[1],
                        "MID PRICE",
                        &format!("${}", fmt_price(mid_fp)),
                        cyan,
                    );

                    let bid_txt = best_bid
                        .map(|(p, q)| format!("${} ({} sh)", fmt_price(p), q))
                        .unwrap_or_else(|| "---".to_string());
                    render_kpi_card(&mut cols[2], "BEST BID", &bid_txt, green);

                    let ask_txt = best_ask
                        .map(|(p, q)| format!("${} ({} sh)", fmt_price(p), q))
                        .unwrap_or_else(|| "---".to_string());
                    render_kpi_card(&mut cols[3], "BEST ASK", &ask_txt, red);

                    let spread_txt = spread_fp
                        .map(|s| format!("${}", fmt_price(s)))
                        .unwrap_or_else(|| "NONE".to_string());
                    render_kpi_card(&mut cols[4], "SPREAD", &spread_txt, amber);

                    let stats_txt = format!(
                        "{} msgs | {} trd ({} sh)",
                        total_itch, total_trades, total_vol
                    );
                    render_kpi_card(&mut cols[5], "ITCH FEED / VOLUME", &stats_txt, Color32::WHITE);
                });
            });

        // ====================================================================
        // RIGHT PANEL: L2 Depth Ladder + Interactive Order Entry Controls
        // ====================================================================
        let mut clicked_book_price: Option<u64> = None;

        egui::SidePanel::right("order_book_panel")
            .resizable(true)
            .default_width(420.0)
            .min_width(360.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);

                // --- 1. L2 ORDER BOOK ---
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("ORDER BOOK (L2 DEPTH)").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        egui::ComboBox::from_id_salt("depth_sel")
                            .selected_text(format!("{} levels", self.depth_levels))
                            .show_ui(ui, |ui| {
                                for d in [5, 8, 10, 12, 16] {
                                    ui.selectable_value(
                                        &mut self.depth_levels,
                                        d,
                                        format!("{d} levels"),
                                    );
                                }
                            });
                    });
                });

                // Book Imbalance Meter
                let total_depth = total_bid_sh + total_ask_sh;
                let bid_ratio = if total_depth > 0 {
                    total_bid_sh as f32 / total_depth as f32
                } else {
                    0.5
                };
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "Bids: {} sh ({:.0}%)",
                            total_bid_sh,
                            bid_ratio * 100.0
                        ))
                        .small()
                        .color(green),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "Asks: {} sh ({:.0}%)",
                                total_ask_sh,
                                (1.0 - bid_ratio) * 100.0
                            ))
                            .small()
                            .color(red),
                        );
                    });
                });

                let (bar_rect, _) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 7.0), egui::Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(bar_rect, CornerRadius::same(3), Color32::from_rgb(90, 30, 35));
                let mut bid_rect = bar_rect;
                bid_rect.set_width(bar_rect.width() * bid_ratio);
                painter.rect_filled(bid_rect, CornerRadius::same(3), Color32::from_rgb(20, 120, 70));

                ui.separator();

                // Book Column Headers
                ui.horizontal(|ui| {
                    ui.add_sized([55.0, 16.0], egui::Label::new(RichText::new("LEVEL").small().color(muted)));
                    ui.add_sized([95.0, 16.0], egui::Label::new(RichText::new("PRICE").small().color(muted)));
                    ui.add_sized([80.0, 16.0], egui::Label::new(RichText::new("SHARES").small().color(muted)));
                    ui.add_sized([85.0, 16.0], egui::Label::new(RichText::new("CUMULATIVE").small().color(muted)));
                });
                ui.separator();

                let mut ask_cum = Vec::with_capacity(top_asks.len());
                let mut running_ask = 0u64;
                for &(p, q) in &top_asks {
                    running_ask += q as u64;
                    ask_cum.push((p, q, running_ask));
                }

                let mut bid_cum = Vec::with_capacity(top_bids.len());
                let mut running_bid = 0u64;
                for &(p, q) in &top_bids {
                    running_bid += q as u64;
                    bid_cum.push((p, q, running_bid));
                }

                let max_cum = running_ask.max(running_bid).max(1) as f32;

                // Render Depth Table
                let missing_asks = self.depth_levels.saturating_sub(ask_cum.len());
                for _ in 0..missing_asks {
                    render_empty_book_row(ui);
                }
                for (idx, &(price, shares, cum)) in ask_cum.iter().enumerate().rev() {
                    if render_book_row(
                        ui,
                        &format!("ASK {}", idx + 1),
                        price,
                        shares,
                        cum,
                        cum as f32 / max_cum,
                        red,
                        Color32::from_rgba_unmultiplied(220, 50, 65, 45),
                    ) {
                        clicked_book_price = Some(price);
                    }
                }

                // SPREAD BANNER
                ui.add_space(2.0);
                egui::Frame::NONE
                    .fill(Color32::from_rgb(30, 35, 45))
                    .corner_radius(CornerRadius::same(4))
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let sp_str = spread_fp
                                .map(|s| format!("Spread: ${}", fmt_price(s)))
                                .unwrap_or_else(|| "Spread: ---".to_string());
                            ui.label(RichText::new(sp_str).strong().color(amber));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    RichText::new(format!("Mid: ${}", fmt_price(mid_fp)))
                                        .strong()
                                        .color(cyan),
                                );
                            });
                        });
                    });
                ui.add_space(2.0);

                for (idx, &(price, shares, cum)) in bid_cum.iter().enumerate() {
                    if render_book_row(
                        ui,
                        &format!("BID {}", idx + 1),
                        price,
                        shares,
                        cum,
                        cum as f32 / max_cum,
                        green,
                        Color32::from_rgba_unmultiplied(0, 190, 100, 45),
                    ) {
                        clicked_book_price = Some(price);
                    }
                }
                let missing_bids = self.depth_levels.saturating_sub(bid_cum.len());
                for _ in 0..missing_bids {
                    render_empty_book_row(ui);
                }

                ui.add_space(6.0);
                ui.separator();

                // --- 2. INTERACTIVE ORDER ENTRY (OUCH 5.0) ---
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("⚡ ORDER ENTRY (OUCH 5.0)").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.ouch_client.is_connected() {
                            ui.label(RichText::new("🟢 OUCH Connected").small().color(green));
                        } else {
                            if ui.button(RichText::new("Connect OUCH").small()).clicked() {
                                let _ = self.ouch_client.connect(&self.ouch_addr);
                            }
                            ui.label(RichText::new("🔴 Disconnected").small().color(red));
                        }
                    });
                });

                if !self.ouch_client.is_connected() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Target:").small().color(muted));
                        ui.text_edit_singleline(&mut self.ouch_addr);
                    });
                }

                ui.add_space(4.0);

                // Buy / Sell Toggle Buttons
                ui.columns(2, |cols| {
                    let buy_active = self.order_side == Side::Buy;
                    let buy_color = if buy_active {
                        green
                    } else {
                        Color32::from_rgb(100, 110, 120)
                    };
                    if cols[0]
                        .add_sized(
                            [cols[0].available_width(), 28.0],
                            egui::Button::new(
                                RichText::new("BUY (BID)").strong().color(if buy_active {
                                    Color32::BLACK
                                } else {
                                    Color32::WHITE
                                }),
                            )
                            .fill(buy_color),
                        )
                        .clicked()
                    {
                        self.order_side = Side::Buy;
                    }

                    let sell_active = self.order_side == Side::Sell;
                    let sell_color = if sell_active {
                        red
                    } else {
                        Color32::from_rgb(100, 110, 120)
                    };
                    if cols[1]
                        .add_sized(
                            [cols[1].available_width(), 28.0],
                            egui::Button::new(
                                RichText::new("SELL (ASK)").strong().color(if sell_active {
                                    Color32::BLACK
                                } else {
                                    Color32::WHITE
                                }),
                            )
                            .fill(sell_color),
                        )
                        .clicked()
                    {
                        self.order_side = Side::Sell;
                    }
                });

                ui.add_space(4.0);

                // Price Input with Steppers & Presets
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Price ($):").strong());
                    ui.add_sized([90.0, 20.0], egui::TextEdit::singleline(&mut self.order_price_str));

                    // Steppers
                    if ui.button("-0.01").clicked() {
                        if let Some(p) = parse_price_fp(&self.order_price_str) {
                            self.order_price_str = fmt_price(p.saturating_sub(100));
                        }
                    }
                    if ui.button("+0.01").clicked() {
                        if let Some(p) = parse_price_fp(&self.order_price_str) {
                            self.order_price_str = fmt_price(p.saturating_add(100));
                        }
                    }

                    // Presets
                    if let Some((bp, _)) = best_bid {
                        if ui.button("Bid").clicked() {
                            self.order_price_str = fmt_price(bp);
                        }
                    }
                    if ui.button("Mid").clicked() {
                        self.order_price_str = fmt_price(mid_fp);
                    }
                    if let Some((ap, _)) = best_ask {
                        if ui.button("Ask").clicked() {
                            self.order_price_str = fmt_price(ap);
                        }
                    }
                });

                // Shares Input with Steppers & Quick Chips
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Shares:   ").strong());
                    let mut shares_str = format!("{}", self.order_shares);
                    if ui
                        .add_sized([90.0, 20.0], egui::TextEdit::singleline(&mut shares_str))
                        .changed()
                    {
                        if let Ok(val) = shares_str.parse::<u32>() {
                            self.order_shares = val;
                        }
                    }

                    if ui.button("-10").clicked() {
                        self.order_shares = self.order_shares.saturating_sub(10).max(1);
                    }
                    if ui.button("+10").clicked() {
                        self.order_shares = self.order_shares.saturating_add(10);
                    }

                    for q in [50, 100, 200, 500] {
                        if ui.button(format!("{q}")).clicked() {
                            self.order_shares = q;
                        }
                    }
                });

                // Notional Value Display
                let parsed_price = parse_price_fp(&self.order_price_str).unwrap_or(0);
                let est_notional =
                    (self.order_shares as f64) * (parsed_price as f64 / PRICE_SCALE as f64);
                ui.label(
                    RichText::new(format!(
                        "Order Value: ${:.2}  (@ ${})",
                        est_notional,
                        fmt_price(parsed_price)
                    ))
                    .small()
                    .color(muted),
                );

                ui.add_space(4.0);

                // Big Submit Button
                let (btn_txt, btn_color) = match self.order_side {
                    Side::Buy => (
                        format!(
                            "🟢 SUBMIT BUY LIMIT ({} sh @ ${})",
                            self.order_shares,
                            fmt_price(parsed_price)
                        ),
                        Color32::from_rgb(0, 175, 95),
                    ),
                    Side::Sell => (
                        format!(
                            "🔴 SUBMIT SELL LIMIT ({} sh @ ${})",
                            self.order_shares,
                            fmt_price(parsed_price)
                        ),
                        Color32::from_rgb(215, 60, 75),
                    ),
                };

                let submit_clicked = ui
                    .add_sized(
                        [ui.available_width(), 32.0],
                        egui::Button::new(
                            RichText::new(btn_txt)
                                .strong()
                                .font(FontId::proportional(14.0)),
                        )
                        .fill(btn_color),
                    )
                    .clicked();

                if submit_clicked {
                    self.submit_order();
                }

                // Feedback Banner
                if let Some((ref msg, is_err)) = self.order_status_feedback {
                    ui.add_space(2.0);
                    let col = if is_err { red } else { green };
                    ui.label(RichText::new(msg).small().color(col));
                }
            });

        // If user clicked any row in the depth ladder, populate that price in order entry!
        if let Some(p) = clicked_book_price {
            self.order_price_str = fmt_price(p);
        }

        // ====================================================================
        // BOTTOM PANEL: Order Blotter / Positions OR Time & Sales (Tabbed)
        // ====================================================================
        egui::TopBottomPanel::bottom("bottom_tab_panel")
            .resizable(true)
            .default_height(210.0)
            .min_height(140.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let active_orders_count = self
                        .user_orders
                        .iter()
                        .filter(|o| {
                            matches!(
                                o.status,
                                OrderStatus::PendingNew
                                    | OrderStatus::Open
                                    | OrderStatus::PartiallyFilled
                            )
                        })
                        .count();

                    let orders_title = if active_orders_count > 0 {
                        format!("⚡ MY ORDERS & POSITION ({active_orders_count} active)")
                    } else {
                        "⚡ MY ORDERS & POSITION".to_string()
                    };

                    if ui
                        .selectable_label(
                            self.bottom_tab == BottomTab::OrdersAndPosition,
                            RichText::new(orders_title).strong(),
                        )
                        .clicked()
                    {
                        self.bottom_tab = BottomTab::OrdersAndPosition;
                    }

                    if ui
                        .selectable_label(
                            self.bottom_tab == BottomTab::TimeAndSales,
                            RichText::new("📜 TIME & SALES (TRADE TAPE)").strong(),
                        )
                        .clicked()
                    {
                        self.bottom_tab = BottomTab::TimeAndSales;
                    }

                    let bot_tab_title = if self.sim_bots.is_empty() {
                        "🤖 BOT PnL".to_string()
                    } else {
                        let best = self
                            .sim_bots
                            .iter()
                            .map(|b| b.state.pnl_snapshot().mtm_pnl_fp)
                            .max()
                            .unwrap_or(0);
                        format!("🤖 BOT PnL (best ${:+.0})", best as f64 / PRICE_SCALE as f64)
                    };
                    if ui
                        .selectable_label(
                            self.bottom_tab == BottomTab::BotPnl,
                            RichText::new(bot_tab_title).strong(),
                        )
                        .clicked()
                    {
                        self.bottom_tab = BottomTab::BotPnl;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "PGM Multicast: RDATA: {} | NAKs: {}",
                                rdata, naks
                            ))
                            .small()
                            .color(muted),
                        );
                    });
                });
                ui.separator();

                match self.bottom_tab {
                    BottomTab::OrdersAndPosition => {
                        // Position & Account Summary Header Bar
                        let pos = self.user_position;
                        let cash_dollars = self.user_cash_fp as f64 / PRICE_SCALE as f64;
                        let mtm_pnl_fp = self.user_cash_fp + pos * (mid_fp as i64);
                        let mtm_pnl_dollars = mtm_pnl_fp as f64 / PRICE_SCALE as f64;

                        let pos_col = if pos > 0 {
                            green
                        } else if pos < 0 {
                            red
                        } else {
                            Color32::WHITE
                        };
                        let pnl_col = if mtm_pnl_dollars > 0.0 {
                            green
                        } else if mtm_pnl_dollars < 0.0 {
                            red
                        } else {
                            Color32::WHITE
                        };

                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Position:").small().color(muted));
                            ui.label(
                                RichText::new(format!("{:+5} sh", pos))
                                    .strong()
                                    .color(pos_col),
                            );

                            ui.separator();
                            ui.label(RichText::new("Cash:").small().color(muted));
                            ui.label(
                                RichText::new(format!("${:.2}", cash_dollars))
                                    .monospace()
                                    .color(Color32::WHITE),
                            );

                            ui.separator();
                            ui.label(RichText::new("MTM PnL:").small().color(muted));
                            ui.label(
                                RichText::new(format!("${:+.2}", mtm_pnl_dollars))
                                    .strong()
                                    .color(pnl_col),
                            );

                            ui.separator();
                            ui.label(
                                RichText::new(format!(
                                    "Fills: {} | Cancels: {}",
                                    self.user_fills, self.user_cancels
                                ))
                                .small()
                                .color(muted),
                            );

                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button(RichText::new("❌ Cancel All Open Orders").small())
                                        .clicked()
                                    {
                                        self.cancel_all_active_orders();
                                    }
                                },
                            );
                        });
                        ui.separator();

                        // Orders Table Headers
                        ui.horizontal(|ui| {
                            ui.add_sized([90.0, 16.0], egui::Label::new(RichText::new("CL_ORD_ID").small().color(muted)));
                            ui.add_sized([80.0, 16.0], egui::Label::new(RichText::new("TIME").small().color(muted)));
                            ui.add_sized([60.0, 16.0], egui::Label::new(RichText::new("SIDE").small().color(muted)));
                            ui.add_sized([95.0, 16.0], egui::Label::new(RichText::new("PRICE").small().color(muted)));
                            ui.add_sized([80.0, 16.0], egui::Label::new(RichText::new("SHARES").small().color(muted)));
                            ui.add_sized([85.0, 16.0], egui::Label::new(RichText::new("REMAINING").small().color(muted)));
                            ui.add_sized([100.0, 16.0], egui::Label::new(RichText::new("STATUS").small().color(muted)));
                            ui.add_sized([90.0, 16.0], egui::Label::new(RichText::new("ACTION").small().color(muted)));
                        });
                        ui.separator();

                        let mut order_to_cancel: Option<u64> = None;

                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if self.user_orders.is_empty() {
                                    ui.add_space(8.0);
                                    ui.label(
                                        RichText::new(
                                            "No orders submitted yet. Use the Order Entry panel on the right.",
                                        )
                                        .italics()
                                        .color(muted),
                                    );
                                } else {
                                    for ord in &self.user_orders {
                                        let (side_txt, side_col) = match ord.side {
                                            Side::Buy => ("BUY", green),
                                            Side::Sell => ("SELL", red),
                                        };
                                        let (status_txt, status_col) = match ord.status {
                                            OrderStatus::PendingNew => ("PENDING", amber),
                                            OrderStatus::Open => ("OPEN", cyan),
                                            OrderStatus::PartiallyFilled => ("PARTIAL", amber),
                                            OrderStatus::Filled => ("FILLED", green),
                                            OrderStatus::Canceled => ("CANCELED", muted),
                                        };

                                        ui.horizontal(|ui| {
                                            ui.add_sized(
                                                [90.0, 18.0],
                                                egui::Label::new(
                                                    RichText::new(format!("#{}", ord.cl_ord_id))
                                                        .monospace(),
                                                ),
                                            );
                                            ui.add_sized(
                                                [80.0, 18.0],
                                                egui::Label::new(
                                                    RichText::new(format!("{:>6.2}s", ord.timestamp_s))
                                                        .monospace()
                                                        .color(muted),
                                                ),
                                            );
                                            ui.add_sized(
                                                [60.0, 18.0],
                                                egui::Label::new(
                                                    RichText::new(side_txt)
                                                        .monospace()
                                                        .strong()
                                                        .color(side_col),
                                                ),
                                            );
                                            ui.add_sized(
                                                [95.0, 18.0],
                                                egui::Label::new(
                                                    RichText::new(format!("${}", fmt_price(ord.price_fp)))
                                                        .monospace()
                                                        .strong(),
                                                ),
                                            );
                                            ui.add_sized(
                                                [80.0, 18.0],
                                                egui::Label::new(
                                                    RichText::new(format!("{:>5} sh", ord.initial_shares))
                                                        .monospace(),
                                                ),
                                            );
                                            ui.add_sized(
                                                [85.0, 18.0],
                                                egui::Label::new(
                                                    RichText::new(format!("{:>5} sh", ord.remaining_shares))
                                                        .monospace()
                                                        .color(if ord.remaining_shares > 0 {
                                                            Color32::WHITE
                                                        } else {
                                                            muted
                                                        }),
                                                ),
                                            );
                                            ui.add_sized(
                                                [100.0, 18.0],
                                                egui::Label::new(
                                                    RichText::new(status_txt)
                                                        .monospace()
                                                        .strong()
                                                        .color(status_col),
                                                ),
                                            );

                                            // Action Cancel Button
                                            let can_cancel = matches!(
                                                ord.status,
                                                OrderStatus::PendingNew
                                                    | OrderStatus::Open
                                                    | OrderStatus::PartiallyFilled
                                            );
                                            if can_cancel {
                                                if ui
                                                    .add_sized(
                                                        [80.0, 18.0],
                                                        egui::Button::new(
                                                            RichText::new("✖ Cancel")
                                                                .small()
                                                                .color(red),
                                                        ),
                                                    )
                                                    .clicked()
                                                {
                                                    order_to_cancel = Some(ord.cl_ord_id);
                                                }
                                            } else {
                                                ui.add_sized(
                                                    [80.0, 18.0],
                                                    egui::Label::new(
                                                        RichText::new("---")
                                                            .small()
                                                            .color(Color32::DARK_GRAY),
                                                    ),
                                                );
                                            }
                                        });
                                    }
                                }
                            });

                        if let Some(id) = order_to_cancel {
                            self.cancel_order(id);
                        }
                    }

                    BottomTab::TimeAndSales => {
                        ui.horizontal(|ui| {
                            ui.add_sized([80.0, 16.0], egui::Label::new(RichText::new("TIME (s)").small().color(muted)));
                            ui.add_sized([80.0, 16.0], egui::Label::new(RichText::new("SEQ #").small().color(muted)));
                            ui.add_sized([90.0, 16.0], egui::Label::new(RichText::new("ORDER REF").small().color(muted)));
                            ui.add_sized([70.0, 16.0], egui::Label::new(RichText::new("SIDE").small().color(muted)));
                            ui.add_sized([90.0, 16.0], egui::Label::new(RichText::new("SHARES").small().color(muted)));
                            ui.add_sized([110.0, 16.0], egui::Label::new(RichText::new("MATCH PRICE").small().color(muted)));
                        });
                        ui.separator();

                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for tr in &recent_trades {
                                    let (side_txt, side_col) = match tr.side {
                                        Side::Buy => ("BUY", green),
                                        Side::Sell => ("SELL", red),
                                    };
                                    ui.horizontal(|ui| {
                                        ui.add_sized(
                                            [80.0, 18.0],
                                            egui::Label::new(
                                                RichText::new(format!("{:>7.3}s", tr.time_s)).monospace(),
                                            ),
                                        );
                                        ui.add_sized(
                                            [80.0, 18.0],
                                            egui::Label::new(
                                                RichText::new(format!("#{}", tr.seq_num))
                                                    .monospace()
                                                    .color(muted),
                                            ),
                                        );
                                        ui.add_sized(
                                            [90.0, 18.0],
                                            egui::Label::new(
                                                RichText::new(format!("#{}", tr.order_ref))
                                                    .monospace()
                                                    .color(muted),
                                                ),
                                        );
                                        ui.add_sized(
                                            [70.0, 18.0],
                                            egui::Label::new(
                                                RichText::new(side_txt).monospace().strong().color(side_col),
                                            ),
                                        );
                                        ui.add_sized(
                                            [90.0, 18.0],
                                            egui::Label::new(
                                                RichText::new(format!("{:>5} sh", tr.shares)).monospace(),
                                            ),
                                        );
                                        ui.add_sized(
                                            [110.0, 18.0],
                                            egui::Label::new(
                                                RichText::new(format!("${}", fmt_price(tr.price_fp)))
                                                    .monospace()
                                                    .strong()
                                                    .color(side_col),
                                            ),
                                        );
                                    });
                                }
                            });
                    }

                    BottomTab::BotPnl => {
                        self.render_bot_pnl_tab(ui, green, red, muted);
                    }
                }
            });

        // ====================================================================
        // CENTRAL PANEL: Real-Time Streaming Price Chart
        // ====================================================================
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(RichText::new("REAL-TIME PRICE CHART").strong());
                ui.separator();
                ui.checkbox(&mut self.show_last_price, "Last Trade");
                ui.checkbox(&mut self.show_mid_price, "Mid Price");
                ui.checkbox(&mut self.show_bid_ask, "Best Bid / Ask");
                ui.checkbox(&mut self.show_trade_markers, "Trade Dots");

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (label, secs) in [
                        ("All", 0.0),
                        ("60s", 60.0),
                        ("30s", 30.0),
                        ("15s", 15.0),
                        ("5s", 5.0),
                    ] {
                        if ui
                            .selectable_label(
                                (self.chart_window_secs - secs).abs() < 0.1,
                                label,
                            )
                            .clicked()
                        {
                            self.chart_window_secs = secs;
                        }
                    }
                    ui.label(RichText::new("Window:").small().color(muted));
                });
            });
            ui.add_space(4.0);

            let max_time = samples.last().map(|s| s.time_s).unwrap_or(0.0);
            let min_time = if self.chart_window_secs > 0.0 {
                (max_time - self.chart_window_secs).max(0.0)
            } else {
                0.0
            };

            let mut last_pts = Vec::new();
            let mut mid_pts = Vec::new();
            let mut bid_pts = Vec::new();
            let mut ask_pts = Vec::new();

            for s in samples.iter().filter(|s| s.time_s >= min_time) {
                if let Some(lp) = s.last_px {
                    last_pts.push([s.time_s, lp]);
                }
                mid_pts.push([s.time_s, s.mid_px]);
                if let Some(bp) = s.bid_px {
                    bid_pts.push([s.time_s, bp]);
                }
                if let Some(ap) = s.ask_px {
                    ask_pts.push([s.time_s, ap]);
                }
            }

            let mut buy_dots = Vec::new();
            let mut sell_dots = Vec::new();
            if self.show_trade_markers {
                for tr in recent_trades.iter().filter(|t| t.time_s >= min_time) {
                    let pt = [tr.time_s, tr.price_fp as f64 / PRICE_SCALE as f64];
                    match tr.side {
                        Side::Buy => buy_dots.push(pt),
                        Side::Sell => sell_dots.push(pt),
                    }
                }
            }

            Plot::new("itch_price_plot")
                .legend(Legend::default())
                .x_axis_label("Time (seconds)")
                .y_axis_label("Price ($)")
                .show(ui, |plot_ui| {
                    if self.show_bid_ask && !bid_pts.is_empty() {
                        plot_ui.line(
                            Line::new(PlotPoints::new(bid_pts))
                                .name("Best Bid")
                                .color(Color32::from_rgba_unmultiplied(0, 210, 120, 150))
                                .width(1.2),
                        );
                    }
                    if self.show_bid_ask && !ask_pts.is_empty() {
                        plot_ui.line(
                            Line::new(PlotPoints::new(ask_pts))
                                .name("Best Ask")
                                .color(Color32::from_rgba_unmultiplied(245, 80, 95, 150))
                                .width(1.2),
                        );
                    }
                    if self.show_mid_price && !mid_pts.is_empty() {
                        plot_ui.line(
                            Line::new(PlotPoints::new(mid_pts))
                                .name("Mid Price")
                                .color(cyan)
                                .width(1.5),
                        );
                    }
                    if self.show_last_price && !last_pts.is_empty() {
                        plot_ui.line(
                            Line::new(PlotPoints::new(last_pts))
                                .name("Last Trade Price")
                                .color(amber)
                                .width(2.5),
                        );
                    }
                    if self.show_trade_markers && !buy_dots.is_empty() {
                        plot_ui.points(
                            Points::new(PlotPoints::new(buy_dots))
                                .name("Buy Fills")
                                .color(green)
                                .radius(3.5),
                        );
                    }
                    if self.show_trade_markers && !sell_dots.is_empty() {
                        plot_ui.points(
                            Points::new(PlotPoints::new(sell_dots))
                                .name("Sell Fills")
                                .color(red)
                                .radius(3.5),
                        );
                    }
                });
        });
    }
}

fn render_kpi_card(ui: &mut egui::Ui, title: &str, value: &str, color: Color32) {
    egui::Frame::NONE
        .fill(Color32::from_rgb(26, 30, 38))
        .stroke(Stroke::new(1.0, Color32::from_rgb(48, 54, 68)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new(title)
                        .font(FontId::proportional(10.5))
                        .color(Color32::from_rgb(145, 155, 170)),
                );
                ui.label(
                    RichText::new(value)
                        .font(FontId::monospace(15.0))
                        .strong()
                        .color(color),
                );
            });
        });
}

#[allow(clippy::too_many_arguments)]
fn render_book_row(
    ui: &mut egui::Ui,
    level_label: &str,
    price_fp: u64,
    shares: u32,
    cum_shares: u64,
    depth_fraction: f32,
    text_color: Color32,
    bar_color: Color32,
) -> bool {
    let row_height = 20.0;
    let desired = Vec2::new(ui.available_width(), row_height);
    let (rect, response) = ui.allocate_exact_size(desired, egui::Sense::click());

    if response.hovered() {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(2),
            Color32::from_rgba_unmultiplied(255, 255, 255, 15),
        );
    }

    let bar_w = rect.width() * depth_fraction.clamp(0.02, 1.0);
    let bar_rect = egui::Rect::from_min_max(
        egui::pos2(rect.max.x - bar_w, rect.min.y + 1.0),
        egui::pos2(rect.max.x, rect.max.y - 1.0),
    );
    ui.painter()
        .rect_filled(bar_rect, CornerRadius::same(2), bar_color);

    let mut child_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child_ui.add_sized(
        [55.0, row_height],
        egui::Label::new(
            RichText::new(level_label)
                .monospace()
                .small()
                .color(Color32::from_rgb(160, 170, 185)),
        ),
    );
    child_ui.add_sized(
        [95.0, row_height],
        egui::Label::new(
            RichText::new(format!("${}", fmt_price(price_fp)))
                .monospace()
                .strong()
                .color(text_color),
        ),
    );
    child_ui.add_sized(
        [80.0, row_height],
        egui::Label::new(
            RichText::new(format!("{:>6} sh", shares))
                .monospace()
                .color(Color32::WHITE),
        ),
    );
    child_ui.add_sized(
        [85.0, row_height],
        egui::Label::new(
            RichText::new(format!("{:>7} sh", cum_shares))
                .monospace()
                .small()
                .color(Color32::from_rgb(165, 175, 190)),
        ),
    );

    response.clicked()
}

fn render_empty_book_row(ui: &mut egui::Ui) {
    let row_height = 20.0;
    let desired = Vec2::new(ui.available_width(), row_height);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());
    let mut child_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    child_ui.add_sized(
        [55.0, row_height],
        egui::Label::new(RichText::new("---").monospace().small().color(Color32::DARK_GRAY)),
    );
}

// ============================================================================
// CLI Fallback (`--cli`) + Main Entrypoint
// ============================================================================

fn run_cli_mode(
    mcast_group: Ipv4Addr,
    mcast_port: u16,
    nak_addr: &str,
    depth: usize,
    refresh_ms: u64,
    duration_ms: Option<u64>,
) -> std::io::Result<()> {
    let mut subscriber = PgmSubscriber::join(mcast_group, mcast_port, nak_addr)?;
    let running = Arc::new(AtomicBool::new(true));
    if let Some(ms) = duration_ms {
        let r = Arc::clone(&running);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(ms));
            r.store(false, Ordering::Relaxed);
        });
    }

    let mut book = ShadowBook::default();
    let mut batch = Vec::with_capacity(64);
    let mut last_print = Instant::now();

    while running.load(Ordering::Relaxed) {
        batch.clear();
        if subscriber.poll_itch(&mut batch)? {
            for msg in batch.drain(..) {
                book.apply(msg);
                if let ItchMessage::OrderExecuted {
                    order_ref,
                    side,
                    executed_shares,
                    match_price,
                    ..
                } = msg
                {
                    println!(
                        "  >>> [TRADE] {:>4} sh @ ${} | {:?} | Ref #{} | Current Price: ${}",
                        executed_shares,
                        fmt_price(match_price),
                        side,
                        order_ref,
                        fmt_price(match_price)
                    );
                }
            }
        } else {
            thread::sleep(Duration::from_millis(1));
        }

        if last_print.elapsed() >= Duration::from_millis(refresh_ms) && book.itch_packets > 0 {
            println!(
                "[BOOK] Last: ${} | Mid: ${} | Bids: {:?} | Asks: {:?}",
                fmt_price(book.last_trade_price),
                fmt_price(book.mid_price()),
                book.top_bids(depth),
                book.top_asks(depth)
            );
            last_print = Instant::now();
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let mut mcast_group = DEFAULT_MCAST_GROUP;
    let mut mcast_port = DEFAULT_MCAST_PORT;
    let mut nak_addr = DEFAULT_NAK_ADDR.to_string();
    let mut ouch_addr = DEFAULT_OUCH_ADDR.to_string();
    let mut depth: usize = 10;
    let mut refresh_ms: u64 = 250;
    let mut duration_ms: Option<u64> = None;
    let mut cli_mode = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cli" => cli_mode = true,
            "--ouch" => {
                if let Some(v) = args.next() {
                    ouch_addr = v;
                }
            }
            "--mcast-group" => {
                if let Some(v) = args.next()
                    && let Ok(ip) = v.parse::<Ipv4Addr>()
                {
                    mcast_group = ip;
                }
            }
            "--mcast-port" => {
                if let Some(v) = args.next()
                    && let Ok(p) = v.parse::<u16>()
                {
                    mcast_port = p;
                }
            }
            "--nak" => {
                if let Some(v) = args.next() {
                    nak_addr = v;
                }
            }
            "--depth" => {
                if let Some(v) = args.next()
                    && let Ok(d) = v.parse::<usize>()
                {
                    depth = d.max(1);
                }
            }
            "--refresh-ms" => {
                if let Some(v) = args.next()
                    && let Ok(ms) = v.parse::<u64>()
                {
                    refresh_ms = ms.max(10);
                }
            }
            "--duration-ms" => {
                if let Some(v) = args.next() {
                    duration_ms = v.parse().ok();
                }
            }
            _ => {}
        }
    }

    if cli_mode {
        return run_cli_mode(
            mcast_group,
            mcast_port,
            &nak_addr,
            depth,
            refresh_ms,
            duration_ms,
        );
    }

    let state = Arc::new(Mutex::new(SharedMarketState::new()));
    let bg_state = Arc::clone(&state);
    let bg_nak = nak_addr.clone();

    // Background PGM ITCH Multicast Receiver Thread
    thread::Builder::new()
        .name("itch-gui-listener".into())
        .spawn(move || {
            let Ok(mut subscriber) = PgmSubscriber::join(mcast_group, mcast_port, &bg_nak) else {
                eprintln!(
                    "[ITCH GUI] Failed to bind multicast subscriber on {}:{}",
                    mcast_group, mcast_port
                );
                return;
            };
            let mut batch = Vec::with_capacity(64);
            loop {
                batch.clear();
                match subscriber.poll_itch(&mut batch) {
                    Ok(true) => {
                        let naks = subscriber.naks_sent;
                        let rdata = subscriber.rdata_recv;
                        if let Ok(mut st) = bg_state.lock() {
                            for msg in batch.drain(..) {
                                st.on_itch(msg, naks, rdata);
                            }
                        }
                    }
                    Ok(false) => thread::sleep(Duration::from_millis(1)),
                    Err(_) => thread::sleep(Duration::from_millis(5)),
                }
            }
        })?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("NASDAQ ITCH 5.0 / OUCH 5.0 — Market Depth, Price Chart & Order Blotter")
            .with_inner_size([1360.0, 840.0])
            .with_min_inner_size([980.0, 620.0]),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "NASDAQ ITCH / OUCH Market Monitor",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(ItchGuiApp::new(
                state,
                mcast_group,
                mcast_port,
                nak_addr,
                ouch_addr,
            )))
        }),
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_price_fp() {
        assert_eq!(parse_price_fp("100"), Some(1_000_000));
        assert_eq!(parse_price_fp("100.25"), Some(1_002_500));
        assert_eq!(parse_price_fp("$100.2500"), Some(1_002_500));
        assert_eq!(parse_price_fp("98.5"), Some(985_000));
        assert_eq!(parse_price_fp("98.1234"), Some(981_234));
        assert_eq!(parse_price_fp("98.12349"), Some(981_234));
        assert_eq!(parse_price_fp(""), None);
        assert_eq!(parse_price_fp("abc"), None);
    }

    #[test]
    fn test_order_roundtrip() {
        let enter = OuchInbound::EnterOrder {
            cl_ord_id: 9_000_001,
            side: Side::Buy,
            shares: 100,
            price: 1_002_500,
        };
        let bytes = enter.to_bytes();
        let decoded = OuchInbound::from_bytes(&bytes).expect("valid frame");
        match decoded {
            OuchInbound::EnterOrder {
                cl_ord_id,
                side,
                shares,
                price,
            } => {
                assert_eq!(cl_ord_id, 9_000_001);
                assert_eq!(side, Side::Buy);
                assert_eq!(shares, 100);
                assert_eq!(price, 1_002_500);
            }
            _ => panic!("wrong variant"),
        }

        let cancel = OuchInbound::CancelOrder {
            cl_ord_id: 9_000_001,
        };
        let c_bytes = cancel.to_bytes();
        let c_decoded = OuchInbound::from_bytes(&c_bytes).expect("valid frame");
        match c_decoded {
            OuchInbound::CancelOrder { cl_ord_id } => {
                assert_eq!(cl_ord_id, 9_000_001);
            }
            _ => panic!("wrong variant"),
        }
    }
}
