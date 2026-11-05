//! Standalone Real-Time ITCH 5.0 Market Data GUI Client (`itch_client`)
//!
//! Uses `eframe` / `egui` + `egui_plot` to render:
//! - Real-Time Streaming Price Chart (Last Trade, Mid Price, Best Bid / Best Ask envelope)
//! - Level-2 Market Depth Order Book Ladder with volume bars & cumulative depth
//! - Live Time & Sales (Trade Tape) and PGM Multicast Telemetry
//! - Optional 1-click Built-in Simulation Traffic Generator (or passive listener for external `sim`/`exchange`)
//!
//! Usage:
//!   cargo run --release --bin itch_client
//!   cargo run --release --bin itch_client -- --cli --duration-ms 2500

use eframe::egui::{self, Color32, CornerRadius, FontId, RichText, Stroke, Vec2};
use egui_plot::{Legend, Line, Plot, PlotPoints, Points};
use itch_ouch_sim::bot::{ShadowBook, default_strategies_for_bot, spawn_bot};
use itch_ouch_sim::exchange::ExchangeServer;
use itch_ouch_sim::pgm::{
    DEFAULT_MCAST_GROUP, DEFAULT_MCAST_PORT, DEFAULT_NAK_ADDR, DEFAULT_OUCH_ADDR, PgmSubscriber,
};
use itch_ouch_sim::protocol::{ItchMessage, PRICE_SCALE, Side, fmt_price};
use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

        // Append sample if trade occurred or enough time elapsed since last sample (or first sample)
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

struct ItchGuiApp {
    state: Arc<Mutex<SharedMarketState>>,
    mcast_group: Ipv4Addr,
    mcast_port: u16,
    nak_addr: String,
    depth_levels: usize,
    chart_window_secs: f64, // 0.0 = All
    show_last_price: bool,
    show_mid_price: bool,
    show_bid_ask: bool,
    show_trade_markers: bool,
    paused: bool,
    demo_sim_running: Option<Arc<AtomicBool>>,
    status_msg: String,
}

impl ItchGuiApp {
    fn new(
        state: Arc<Mutex<SharedMarketState>>,
        mcast_group: Ipv4Addr,
        mcast_port: u16,
        nak_addr: String,
    ) -> Self {
        Self {
            state,
            mcast_group,
            mcast_port,
            nak_addr,
            depth_levels: 12,
            chart_window_secs: 15.0,
            show_last_price: true,
            show_mid_price: true,
            show_bid_ask: true,
            show_trade_markers: true,
            paused: false,
            demo_sim_running: None,
            status_msg: "Listening on PGM Multicast".to_string(),
        }
    }

    fn toggle_builtin_sim(&mut self) {
        if let Some(flag) = self.demo_sim_running.take() {
            flag.store(false, Ordering::Relaxed);
            self.status_msg = "Stopped built-in simulation".to_string();
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

                let (name1, strat1) = default_strategies_for_bot(1);
                let _ = spawn_bot(
                    name1,
                    1,
                    &ouch_addr,
                    mcast_group,
                    mcast_port,
                    &nak_addr,
                    strat1,
                    Arc::clone(&running),
                );

                let (name2, strat2) = default_strategies_for_bot(2);
                let _ = spawn_bot(
                    name2,
                    2,
                    &ouch_addr,
                    mcast_group,
                    mcast_port,
                    &nak_addr,
                    strat2,
                    Arc::clone(&running),
                );

                self.demo_sim_running = Some(sim_flag);
                self.status_msg = format!("Built-in Exchange + 2 Bots running on {ouch_addr}");
            }
            Err(e) => {
                self.status_msg = format!(
                    "External Exchange already active or port busy ({e}) — listening passively"
                );
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

        // Snapshot current state under lock quickly
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
                        RichText::new("▶ Start Built-in Sim (Exchange + 2 Bots)")
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

                    if ui.button("🗑 Reset").clicked() {
                        self.state.lock().unwrap().reset();
                    }

                    ui.separator();
                    ui.label(RichText::new(&self.status_msg).small().color(cyan));
                });

                ui.add_space(6.0);

                // KPI Cards Row
                ui.columns(6, |cols| {
                    // 1. Last Trade Price
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

                    // 2. Mid Price
                    render_kpi_card(
                        &mut cols[1],
                        "MID PRICE",
                        &format!("${}", fmt_price(mid_fp)),
                        cyan,
                    );

                    // 3. Best Bid
                    let bid_txt = best_bid
                        .map(|(p, q)| format!("${} ({} sh)", fmt_price(p), q))
                        .unwrap_or_else(|| "---".to_string());
                    render_kpi_card(&mut cols[2], "BEST BID", &bid_txt, green);

                    // 4. Best Ask
                    let ask_txt = best_ask
                        .map(|(p, q)| format!("${} ({} sh)", fmt_price(p), q))
                        .unwrap_or_else(|| "---".to_string());
                    render_kpi_card(&mut cols[3], "BEST ASK", &ask_txt, red);

                    // 5. Spread
                    let spread_txt = spread_fp
                        .map(|s| format!("${}", fmt_price(s)))
                        .unwrap_or_else(|| "NONE".to_string());
                    render_kpi_card(&mut cols[4], "SPREAD", &spread_txt, amber);

                    // 6. Feed & Trade Volume
                    let stats_txt = format!(
                        "{} msgs | {} trd ({} sh)",
                        total_itch, total_trades, total_vol
                    );
                    render_kpi_card(&mut cols[5], "ITCH FEED / VOLUME", &stats_txt, Color32::WHITE);
                });
            });

        // ====================================================================
        // RIGHT PANEL: Level-2 Market Depth Ladder (Order Book)
        // ====================================================================
        egui::SidePanel::right("order_book_panel")
            .resizable(true)
            .default_width(400.0)
            .min_width(340.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("ORDER BOOK (L2 DEPTH)").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        egui::ComboBox::from_id_salt("depth_sel")
                            .selected_text(format!("{} levels", self.depth_levels))
                            .show_ui(ui, |ui| {
                                for d in [5, 8, 12, 16, 20] {
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
                ui.add_space(4.0);
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
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), egui::Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(bar_rect, CornerRadius::same(3), Color32::from_rgb(90, 30, 35));
                let mut bid_rect = bar_rect;
                bid_rect.set_width(bar_rect.width() * bid_ratio);
                painter.rect_filled(bid_rect, CornerRadius::same(3), Color32::from_rgb(20, 120, 70));

                ui.separator();

                // Column Headers
                ui.horizontal(|ui| {
                    ui.add_sized([55.0, 18.0], egui::Label::new(RichText::new("LEVEL").small().color(muted)));
                    ui.add_sized([95.0, 18.0], egui::Label::new(RichText::new("PRICE").small().color(muted)));
                    ui.add_sized([80.0, 18.0], egui::Label::new(RichText::new("SHARES").small().color(muted)));
                    ui.add_sized([85.0, 18.0], egui::Label::new(RichText::new("CUMULATIVE").small().color(muted)));
                });
                ui.separator();

                // Compute cumulative shares and max level size for depth shading
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

                egui::ScrollArea::vertical().show(ui, |ui| {
                    // ASKS: displayed from highest price (top) down to Best Ask (bottom, just above spread)
                    let missing_asks = self.depth_levels.saturating_sub(ask_cum.len());
                    for _ in 0..missing_asks {
                        render_empty_book_row(ui);
                    }
                    for (idx, &(price, shares, cum)) in ask_cum.iter().enumerate().rev() {
                        render_book_row(
                            ui,
                            &format!("ASK {}", idx + 1),
                            price,
                            shares,
                            cum,
                            cum as f32 / max_cum,
                            red,
                            Color32::from_rgba_unmultiplied(220, 50, 65, 45),
                        );
                    }

                    // SPREAD BANNER
                    ui.add_space(3.0);
                    egui::Frame::NONE
                        .fill(Color32::from_rgb(30, 35, 45))
                        .corner_radius(CornerRadius::same(4))
                        .inner_margin(egui::Margin::symmetric(8, 5))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let sp_str = spread_fp
                                    .map(|s| format!("Spread: ${}", fmt_price(s)))
                                    .unwrap_or_else(|| "Spread: ---".to_string());
                                ui.label(RichText::new(sp_str).strong().color(amber));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            RichText::new(format!("Mid: ${}", fmt_price(mid_fp)))
                                                .strong()
                                                .color(cyan),
                                        );
                                    },
                                );
                            });
                        });
                    ui.add_space(3.0);

                    // BIDS: displayed from Best Bid (top, just below spread) down to lower bids
                    for (idx, &(price, shares, cum)) in bid_cum.iter().enumerate() {
                        render_book_row(
                            ui,
                            &format!("BID {}", idx + 1),
                            price,
                            shares,
                            cum,
                            cum as f32 / max_cum,
                            green,
                            Color32::from_rgba_unmultiplied(0, 190, 100, 45),
                        );
                    }
                    let missing_bids = self.depth_levels.saturating_sub(bid_cum.len());
                    for _ in 0..missing_bids {
                        render_empty_book_row(ui);
                    }
                });
            });

        // ====================================================================
        // BOTTOM PANEL: Live Time & Sales (Trade Tape)
        // ====================================================================
        egui::TopBottomPanel::bottom("trade_tape_panel")
            .resizable(true)
            .default_height(185.0)
            .min_height(110.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new("TIME & SALES (LIVE TRADE EXECUTIONS)").strong());
                    ui.separator();
                    ui.label(
                        RichText::new(format!(
                            "PGM Reliability: RDATA Recovered: {} | NAKs Sent: {}",
                            rdata, naks
                        ))
                        .small()
                        .color(muted),
                    );
                });
                ui.separator();

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
) {
    let row_height = 20.0;
    let desired = Vec2::new(ui.available_width(), row_height);
    let (rect, _) = ui.allocate_exact_size(desired, egui::Sense::hover());

    // Draw right-aligned depth bar proportional to cumulative depth
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
// Optional CLI Fallback (`--cli`) + Main Entrypoint
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
    let mut depth: usize = 5;
    let mut refresh_ms: u64 = 250;
    let mut duration_ms: Option<u64> = None;
    let mut cli_mode = false;
    let _ = DEFAULT_OUCH_ADDR;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cli" => cli_mode = true,
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
            .with_title("NASDAQ ITCH 5.0 — Order Book & Real-Time Price Chart")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "NASDAQ ITCH 5.0 Market Monitor",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(ItchGuiApp::new(
                state,
                mcast_group,
                mcast_port,
                nak_addr,
            )))
        }),
    );

    Ok(())
}
