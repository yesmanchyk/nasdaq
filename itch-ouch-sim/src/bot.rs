//! Trading Bot with PGM Multicast ITCH Listener, TCP OUCH Order Gateway,
//! and Two Concurrent Strategy Threads per Bot.
//!
//! Architecture inside each Bot:
//! 1. PGM ITCH Listener Thread: Joins IPv4 Multicast (`239.255.0.1:30001`),
//!    tracks sequence continuity, issues RFC 3208 `PGM_NAK` on gaps, and updates
//!    a shared `RwLock<ShadowBook>`.
//! 2. TCP OUCH Reader Thread: Reads execution reports & acks over TCP (`TCP_NODELAY`),
//!    updating real-time inventory and cash PnL.
//! 3. TCP OUCH Writer Thread: Drains an `mpsc::Receiver<OuchInbound>` from the bot's
//!    concurrent strategies so TCP OUCH frames are never interleaved.
//! 4. Strategy Thread A & Strategy Thread B: Run concurrently, reading the shared
//!    market state and firing `OuchInbound` commands down the MPSC channel.

use crate::pgm::PgmSubscriber;
use crate::protocol::{
    ItchMessage, OuchInbound, OuchOutbound, PRICE_SCALE, Side, fmt_price, try_read_frame,
    write_frame,
};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{Error, ErrorKind};
use std::net::{Ipv4Addr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// Shadow order book reconstructed from the exchange's PGM ITCH feed
#[derive(Default)]
pub struct ShadowBook {
    /// order_ref -> (Side, price, shares)
    orders: HashMap<u64, (Side, u64, u32)>,
    /// price -> aggregate shares
    bids: BTreeMap<u64, u32>,
    asks: BTreeMap<u64, u32>,
    pub last_trade_price: u64,
    pub recent_buy_exec_vol: u64,
    pub recent_sell_exec_vol: u64,
    pub itch_packets: u64,
    pub pgm_rdata_recovered: u64,
    pub pgm_naks_sent: u64,
    /// Exponentially weighted moving average of mid-price (in fixed-point units)
    pub ewma_mid_fp: f64,
    /// Exponentially weighted moving variance of mid-price changes (in fixed-point units^2)
    pub ewma_var_fp: f64,
    /// Exponentially weighted signed aggressive order flow (+buy, -sell shares)
    pub ewma_signed_flow: f64,
    /// Exponentially weighted absolute aggressive order flow (total shares)
    pub ewma_abs_flow: f64,
}

impl ShadowBook {
    pub fn apply(&mut self, msg: ItchMessage) {
        self.itch_packets += 1;
        match msg {
            ItchMessage::OrderAdded {
                order_ref,
                side,
                shares,
                price,
                ..
            } => {
                self.orders.insert(order_ref, (side, price, shares));
                let book_side = match side {
                    Side::Buy => &mut self.bids,
                    Side::Sell => &mut self.asks,
                };
                *book_side.entry(price).or_insert(0) += shares;
            }
            ItchMessage::OrderExecuted {
                order_ref,
                side,
                executed_shares,
                match_price,
                ..
            } => {
                self.last_trade_price = match_price;
                let qty_f = executed_shares as f64;
                // Decay flow EWMAs (alpha = 0.12)
                const FLOW_DECAY: f64 = 0.88;
                self.ewma_signed_flow *= FLOW_DECAY;
                self.ewma_abs_flow = self.ewma_abs_flow * FLOW_DECAY + qty_f;

                match side {
                    // Resting Sell order executed -> aggressive Buy trade lifted the ask
                    Side::Sell => {
                        self.recent_buy_exec_vol += executed_shares as u64;
                        self.ewma_signed_flow += qty_f;
                    }
                    // Resting Buy order executed -> aggressive Sell trade hit the bid
                    Side::Buy => {
                        self.recent_sell_exec_vol += executed_shares as u64;
                        self.ewma_signed_flow -= qty_f;
                    }
                }
                self.reduce_order(order_ref, executed_shares);
            }
            ItchMessage::OrderDeleted { order_ref, .. } => {
                self.remove_order(order_ref);
            }
        }

        // Update EWMA mid-price and realized volatility estimate
        let current_mid = self.mid_price() as f64;
        if self.ewma_mid_fp <= 0.0 {
            self.ewma_mid_fp = current_mid;
            self.ewma_var_fp = 25.0; // Initial variance prior (~5 fp units std dev)
        } else {
            let diff = current_mid - self.ewma_mid_fp;
            const ALPHA_MID: f64 = 0.08;
            const ALPHA_VAR: f64 = 0.08;
            self.ewma_mid_fp += ALPHA_MID * diff;
            self.ewma_var_fp = (1.0 - ALPHA_VAR) * self.ewma_var_fp + ALPHA_VAR * (diff * diff);
        }
    }

    fn reduce_order(&mut self, order_ref: u64, delta: u32) {
        if let Some((side, price, rem)) = self.orders.get_mut(&order_ref) {
            let side_val = *side;
            let price_val = *price;
            let sub = (*rem).min(delta);
            *rem -= sub;
            if *rem == 0 {
                self.orders.remove(&order_ref);
            }
            let book_side = match side_val {
                Side::Buy => &mut self.bids,
                Side::Sell => &mut self.asks,
            };
            if let Some(level_qty) = book_side.get_mut(&price_val) {
                *level_qty = level_qty.saturating_sub(sub);
                if *level_qty == 0 {
                    book_side.remove(&price_val);
                }
            }
        }
    }

    fn remove_order(&mut self, order_ref: u64) {
        if let Some((side, price, shares)) = self.orders.remove(&order_ref) {
            let book_side = match side {
                Side::Buy => &mut self.bids,
                Side::Sell => &mut self.asks,
            };
            if let Some(level_qty) = book_side.get_mut(&price) {
                *level_qty = level_qty.saturating_sub(shares);
                if *level_qty == 0 {
                    book_side.remove(&price);
                }
            }
        }
    }

    pub fn best_bid(&self) -> Option<(u64, u32)> {
        self.bids.iter().next_back().map(|(&p, &q)| (p, q))
    }

    pub fn best_ask(&self) -> Option<(u64, u32)> {
        self.asks.iter().next().map(|(&p, &q)| (p, q))
    }

    pub fn spread(&self) -> Option<u64> {
        match (self.best_bid(), self.best_ask()) {
            (Some((b, _)), Some((a, _))) if a >= b => Some(a - b),
            _ => None,
        }
    }

    pub fn top_bids(&self, depth: usize) -> Vec<(u64, u32)> {
        self.bids.iter().rev().take(depth).map(|(&p, &q)| (p, q)).collect()
    }

    pub fn top_asks(&self, depth: usize) -> Vec<(u64, u32)> {
        self.asks.iter().take(depth).map(|(&p, &q)| (p, q)).collect()
    }

    pub fn total_bid_shares(&self) -> u64 {
        self.bids.values().map(|&q| q as u64).sum()
    }

    pub fn total_ask_shares(&self) -> u64 {
        self.asks.values().map(|&q| q as u64).sum()
    }

    pub fn mid_price(&self) -> u64 {
        match (self.best_bid(), self.best_ask()) {
            (Some((b, _)), Some((a, _))) => (b + a) / 2,
            (Some((b, _)), None) => b + 50,
            (None, Some((a, _))) => a.saturating_sub(50),
            (None, None) => {
                if self.last_trade_price > 0 {
                    self.last_trade_price
                } else {
                    100 * PRICE_SCALE // Default $100.0000 anchor
                }
            }
        }
    }

    /// Volume-weighted top-of-book microprice:
    /// `(bid_px * ask_qty + ask_px * bid_qty) / (bid_qty + ask_qty)`
    pub fn micro_price(&self) -> u64 {
        match (self.best_bid(), self.best_ask()) {
            (Some((b_px, b_qty)), Some((a_px, a_qty))) => {
                let total = (b_qty as u64) + (a_qty as u64);
                if total == 0 {
                    (b_px + a_px) / 2
                } else {
                    (b_px * (a_qty as u64) + a_px * (b_qty as u64)) / total
                }
            }
            _ => self.mid_price(),
        }
    }

    /// Multi-level exponentially weighted Order Book Imbalance (OBI) in `[-1.0, +1.0]`.
    /// Positive values indicate deeper bid support relative to ask supply.
    pub fn order_book_imbalance(&self, levels: usize) -> f64 {
        const WEIGHTS: [f64; 5] = [1.0, 0.6065, 0.3679, 0.2231, 0.1353]; // exp(-0.5 * i)
        let mut weighted_bid = 0.0;
        let mut weighted_ask = 0.0;

        for (i, (_, qty)) in self.bids.iter().rev().take(levels).enumerate() {
            let w = WEIGHTS.get(i).copied().unwrap_or(0.1);
            weighted_bid += (*qty as f64) * w;
        }
        for (i, (_, qty)) in self.asks.iter().take(levels).enumerate() {
            let w = WEIGHTS.get(i).copied().unwrap_or(0.1);
            weighted_ask += (*qty as f64) * w;
        }

        let denom = weighted_bid + weighted_ask;
        if denom <= f64::EPSILON {
            0.0
        } else {
            ((weighted_bid - weighted_ask) / denom).clamp(-1.0, 1.0)
        }
    }

    /// Normalized EWMA order-flow toxicity / directional flow imbalance in `[-1.0, +1.0]`.
    /// `+1.0` means one-sided aggressive buying; `-1.0` means one-sided aggressive selling.
    pub fn order_flow_toxicity(&self) -> f64 {
        if self.ewma_abs_flow <= 1.0 {
            0.0
        } else {
            (self.ewma_signed_flow / self.ewma_abs_flow).clamp(-1.0, 1.0)
        }
    }

    /// Instantaneous EWMA volatility (standard deviation of mid-price changes in fixed-point units).
    pub fn ewma_volatility_fp(&self) -> f64 {
        self.ewma_var_fp.max(1.0).sqrt()
    }

    /// Composite microstructure fair value (in fixed-point units) combining:
    /// - Top-of-book volume-weighted microprice
    /// - 5-level decaying Order Book Imbalance (OBI)
    /// - Short-term EWMA signed trade flow pressure
    pub fn smart_fair_value(&self) -> u64 {
        let micro = self.micro_price() as f64;
        let half_spread = self.spread().unwrap_or(20).clamp(4, 120) as f64 * 0.5;
        let obi = self.order_book_imbalance(5);
        let flow = self.order_flow_toxicity();
        // Blend multi-level OBI and trade flow toxicity as fraction of half-spread
        let alpha_shift = half_spread * (0.45 * obi + 0.35 * flow);
        (micro + alpha_shift).round().max(10_000.0) as u64
    }
}

/// Equal, fixed starting capital allocated to every bot ($100,000.00)
pub const INITIAL_BOT_CAPITAL_DOLLARS: u64 = 100_000;
/// Equal, fixed starting capital in fixed-point units (1/10_000 of a dollar)
pub const INITIAL_BOT_CAPITAL_FP: i64 = (INITIAL_BOT_CAPITAL_DOLLARS * PRICE_SCALE) as i64;

/// Point-in-time PnL snapshot of a bot (all monetary values in fixed-point 1/10_000 dollars)
#[derive(Clone, Copy, Debug, Default)]
pub struct BotPnl {
    pub initial_capital_fp: i64,
    pub position: i64,
    pub cash_fp: i64,
    pub mark_fp: u64,
    pub equity_fp: i64,
    pub mtm_pnl_fp: i64,
    pub fills: u64,
    pub acks: u64,
    pub cancels: u64,
    pub rl_q_updates: u64,
    pub rl_bandit_updates: u64,
}

/// Shared bot telemetry & position tracking across its concurrent strategies
pub struct BotState {
    pub name: String,
    pub book: RwLock<ShadowBook>,
    /// Fixed starting capital in fixed-point units (1/10_000 of a dollar)
    pub initial_capital_fp: i64,
    pub position: AtomicI64,
    /// Available cash balance in fixed-point units (1/10_000 of a dollar)
    pub cash_fp: AtomicI64,
    pub fills_count: AtomicU64,
    pub acks_count: AtomicU64,
    pub cancels_count: AtomicU64,
    pub rl_q_updates: AtomicU64,
    pub rl_bandit_updates: AtomicU64,
    next_cl_ord_id: AtomicU64,
}

impl BotState {
    pub fn new(name: impl Into<String>, bot_id_prefix: u64) -> Self {
        Self::with_capital(name, bot_id_prefix, INITIAL_BOT_CAPITAL_FP)
    }

    pub fn with_capital(
        name: impl Into<String>,
        bot_id_prefix: u64,
        initial_capital_fp: i64,
    ) -> Self {
        Self {
            name: name.into(),
            book: RwLock::new(ShadowBook::default()),
            initial_capital_fp,
            position: AtomicI64::new(0),
            cash_fp: AtomicI64::new(initial_capital_fp),
            fills_count: AtomicU64::new(0),
            acks_count: AtomicU64::new(0),
            cancels_count: AtomicU64::new(0),
            rl_q_updates: AtomicU64::new(0),
            rl_bandit_updates: AtomicU64::new(0),
            next_cl_ord_id: AtomicU64::new(bot_id_prefix * 1_000_000 + 1),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_cl_ord_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Clamps requested order size to what the bot can afford within its fixed capital budget (1x equity).
    pub fn affordable_shares(&self, side: Side, requested_shares: u32, price: u64) -> u32 {
        if price == 0 || requested_shares == 0 {
            return 0;
        }
        let px = price as i64;
        let cash = self.cash_fp.load(Ordering::Relaxed);
        let pos = self.position.load(Ordering::Relaxed);
        let equity_fp = (cash + pos * px).max(0);
        let max_exposure_shares = equity_fp / px;

        match side {
            Side::Buy => {
                let max_by_cash = (cash.max(0) / px).max(0);
                let max_by_equity = (max_exposure_shares - pos).max(0);
                let max_sh = max_by_cash.min(max_by_equity) as u32;
                requested_shares.min(max_sh)
            }
            Side::Sell => {
                let max_by_equity = (max_exposure_shares + pos).max(0) as u32;
                requested_shares.min(max_by_equity)
            }
        }
    }

    /// Point-in-time PnL / activity snapshot, marked to the bot's own shadow-book mid price.
    pub fn pnl_snapshot(&self) -> BotPnl {
        let mark_fp = self.book.read().unwrap().mid_price();
        let position = self.position.load(Ordering::Relaxed);
        let cash_fp = self.cash_fp.load(Ordering::Relaxed);
        let equity_fp = cash_fp + position * (mark_fp as i64);
        let mtm_pnl_fp = equity_fp - self.initial_capital_fp;
        BotPnl {
            initial_capital_fp: self.initial_capital_fp,
            position,
            cash_fp,
            mark_fp,
            equity_fp,
            mtm_pnl_fp,
            fills: self.fills_count.load(Ordering::Relaxed),
            acks: self.acks_count.load(Ordering::Relaxed),
            cancels: self.cancels_count.load(Ordering::Relaxed),
            rl_q_updates: self.rl_q_updates.load(Ordering::Relaxed),
            rl_bandit_updates: self.rl_bandit_updates.load(Ordering::Relaxed),
        }
    }

    pub fn summary(&self) -> String {
        let book = self.book.read().unwrap();
        let mid = book.mid_price();
        let pos = self.position.load(Ordering::Relaxed);
        let cash = self.cash_fp.load(Ordering::Relaxed);
        let equity_fp = cash + pos * (mid as i64);
        let mtm_pnl = equity_fp - self.initial_capital_fp;
        let equity_dollars = equity_fp as f64 / (PRICE_SCALE as f64);
        let pnl_dollars = mtm_pnl as f64 / (PRICE_SCALE as f64);
        let rl_q = self.rl_q_updates.load(Ordering::Relaxed);
        let rl_b = self.rl_bandit_updates.load(Ordering::Relaxed);
        let rl_suffix = if rl_q > 0 || rl_b > 0 {
            format!(" | RL_Steps[Q:{rl_q} B:{rl_b}]")
        } else {
            String::new()
        };

        format!(
            "[{}] PGM_ITCH: {:4} (RDATA:{}/NAK:{}) | Acks: {:4} | Cancels: {:4} | Fills: {:4} | Pos: {:+5} sh | Equity: ${:10.2} | MTM PnL: ${:+9.2}{}",
            self.name,
            book.itch_packets,
            book.pgm_rdata_recovered,
            book.pgm_naks_sent,
            self.acks_count.load(Ordering::Relaxed),
            self.cancels_count.load(Ordering::Relaxed),
            self.fills_count.load(Ordering::Relaxed),
            pos,
            equity_dollars,
            pnl_dollars,
            rl_suffix
        )
    }
}

#[derive(Clone, Copy)]
pub enum StrategyKind {
    /// Passive two-sided Market Maker quoting around mid-price with inventory skew
    MarketMaker { half_spread: u64, lot_size: u32 },
    /// Mean-reversion strategy fading deviations from fair value ($100.0000)
    MeanReversion { fair_value: u64, threshold: u64 },
    /// Momentum sniper reacting to order-flow imbalance in ITCH executions
    MomentumSniper { lot_size: u32 },
    /// Periodic liquidity sweep / taker probing top of book
    LiquidityTaker { lot_size: u32 },
    /// Smart Avellaneda-Stoikov Adaptive Market Maker with reservation price,
    /// realized volatility scaling, and order-flow toxicity protection
    AvellanedaStoikovMM {
        risk_aversion_bps: u64,
        base_half_spread: u64,
        lot_size: u32,
        max_inventory: i64,
    },
    /// Smart Microprice & Multi-Level Order-Book Imbalance (OBI) Alpha Sniper
    /// with inventory-aware execution and queue-reactive limit orders
    MicropriceImbalanceAlpha {
        edge_threshold: u64,
        lot_size: u32,
        max_inventory: i64,
    },
    /// Online Tabular Q-Learning Market Maker learning optimal quote spreads & skews
    /// across 45 (inventory × OBI × volatility/toxicity) states via Bellman TD(0) updates
    TabularQLearningMM {
        lot_size: u32,
        max_inventory: i64,
        learning_rate_bps: u64,
        discount_bps: u64,
    },
    /// Online Contextual UCB1 Bandit learning when to snipe ask, hit bid, peg BBO, or hold
    /// across 9 (order-book imbalance × order-flow toxicity) market regimes
    ContextualBanditTaker {
        lot_size: u32,
        max_inventory: i64,
        exploration_bps: u64,
    },
}

pub fn default_strategies_for_bot(bot_id: u64) -> (&'static str, [StrategyKind; 2]) {
    match bot_id {
        1 => (
            "Bot-1 (MM + MeanRev)",
            [
                StrategyKind::MarketMaker {
                    half_spread: 25,
                    lot_size: 100,
                },
                StrategyKind::MeanReversion {
                    fair_value: 100 * PRICE_SCALE,
                    threshold: 30,
                },
            ],
        ),
        2 => (
            "Bot-2 (Mom + Taker) ",
            [
                StrategyKind::MomentumSniper { lot_size: 40 },
                StrategyKind::LiquidityTaker { lot_size: 60 },
            ],
        ),
        3 => (
            "Bot-3 (Smart AS+OBI)",
            [
                StrategyKind::AvellanedaStoikovMM {
                    risk_aversion_bps: 15,
                    base_half_spread: 14,
                    lot_size: 80,
                    max_inventory: 600,
                },
                StrategyKind::MicropriceImbalanceAlpha {
                    edge_threshold: 8,
                    lot_size: 50,
                    max_inventory: 600,
                },
            ],
        ),
        _ => (
            "Bot-4 (RL Q+Bandit) ",
            [
                StrategyKind::TabularQLearningMM {
                    lot_size: 75,
                    max_inventory: 600,
                    learning_rate_bps: 1800, // alpha = 0.18
                    discount_bps: 9200,      // gamma = 0.92
                },
                StrategyKind::ContextualBanditTaker {
                    lot_size: 45,
                    max_inventory: 600,
                    exploration_bps: 1200, // c = 0.12
                },
            ],
        ),
    }
}

pub struct BotHandle {
    pub state: Arc<BotState>,
    threads: Vec<JoinHandle<()>>,
}

impl BotHandle {
    pub fn join(self) {
        for t in self.threads {
            let _ = t.join();
        }
    }
}

pub fn spawn_bot(
    name: &str,
    bot_id: u64,
    ouch_tcp_addr: &str,
    pgm_mcast_group: Ipv4Addr,
    pgm_mcast_port: u16,
    pgm_nak_addr: &str,
    strategies: [StrategyKind; 2],
    running: Arc<AtomicBool>,
) -> std::io::Result<BotHandle> {
    let state = Arc::new(BotState::new(name, bot_id));

    // Join PGM Multicast group before connecting TCP OUCH
    let mut pgm_sub = PgmSubscriber::join(pgm_mcast_group, pgm_mcast_port, pgm_nak_addr)?;

    // Connect TCP OUCH stream with TCP_NODELAY
    let ouch_stream = TcpStream::connect(ouch_tcp_addr)?;
    ouch_stream.set_nodelay(true)?;
    let mut ouch_reader = ouch_stream.try_clone()?;
    ouch_reader.set_nonblocking(true)?;
    let mut ouch_writer = ouch_stream;

    let (ouch_tx, ouch_rx) = mpsc::channel::<OuchInbound>();
    let mut threads = Vec::new();

    // 1. PGM Multicast ITCH Receiver Thread
    {
        let state = Arc::clone(&state);
        let running = Arc::clone(&running);
        threads.push(thread::spawn(move || {
            let mut batch = Vec::with_capacity(16);
            while running.load(Ordering::Relaxed) {
                batch.clear();
                match pgm_sub.poll_itch(&mut batch) {
                    Ok(true) => {
                        if let Ok(mut book) = state.book.write() {
                            for msg in batch.drain(..) {
                                book.apply(msg);
                            }
                            book.pgm_rdata_recovered = pgm_sub.rdata_recv;
                            book.pgm_naks_sent = pgm_sub.naks_sent;
                        }
                    }
                    Ok(false) => {
                        thread::sleep(Duration::from_micros(50));
                    }
                    Err(_) => break,
                }
            }
        }));
    }

    // 2. TCP OUCH Execution Report Reader Thread
    {
        let state = Arc::clone(&state);
        let running = Arc::clone(&running);
        threads.push(thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match try_read_frame::<{ OuchOutbound::WIRE_SIZE }>(&mut ouch_reader) {
                    Ok(Some(raw)) => {
                        if let Some(msg) = OuchOutbound::from_bytes(&raw) {
                            match msg {
                                OuchOutbound::OrderAccepted { .. } => {
                                    state.acks_count.fetch_add(1, Ordering::Relaxed);
                                }
                                OuchOutbound::OrderCanceled { .. } => {
                                    state.cancels_count.fetch_add(1, Ordering::Relaxed);
                                }
                                OuchOutbound::OrderExecuted {
                                    side,
                                    executed_shares,
                                    fill_price,
                                    ..
                                } => {
                                    state.fills_count.fetch_add(1, Ordering::Relaxed);
                                    let qty = executed_shares as i64;
                                    let notional = qty * (fill_price as i64);
                                    match side {
                                        Side::Buy => {
                                            state.position.fetch_add(qty, Ordering::Relaxed);
                                            state.cash_fp.fetch_sub(notional, Ordering::Relaxed);
                                        }
                                        Side::Sell => {
                                            state.position.fetch_sub(qty, Ordering::Relaxed);
                                            state.cash_fp.fetch_add(notional, Ordering::Relaxed);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => thread::sleep(Duration::from_micros(50)),
                    Err(_) => break,
                }
            }
        }));
    }

    // 3. TCP OUCH Order Gateway Writer Thread (serializes orders from concurrent strategies
    //    and enforces the bot's fixed starting capital / buying-power budget)
    {
        let state = Arc::clone(&state);
        let running = Arc::clone(&running);
        threads.push(thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match ouch_rx.recv_timeout(Duration::from_millis(5)) {
                    Ok(cmd) => {
                        let cmd = match cmd {
                            OuchInbound::EnterOrder {
                                cl_ord_id,
                                side,
                                shares,
                                price,
                            } => {
                                let allowed = state.affordable_shares(side, shares, price);
                                if allowed == 0 {
                                    continue;
                                }
                                OuchInbound::EnterOrder {
                                    cl_ord_id,
                                    side,
                                    shares: allowed,
                                    price,
                                }
                            }
                            other => other,
                        };
                        let bytes = cmd.to_bytes();
                        if write_frame(&mut ouch_writer, &bytes).is_err() {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        }));
    }

    // 4. Spawn Two Concurrent Strategy Threads
    for (idx, strat) in strategies.into_iter().enumerate() {
        let state = Arc::clone(&state);
        let tx = ouch_tx.clone();
        let running = Arc::clone(&running);
        threads.push(thread::spawn(move || {
            run_strategy(idx, strat, state, tx, running);
        }));
    }

    Ok(BotHandle { state, threads })
}

fn run_strategy(
    strat_idx: usize,
    kind: StrategyKind,
    state: Arc<BotState>,
    tx: mpsc::Sender<OuchInbound>,
    running: Arc<AtomicBool>,
) {
    let mut active_orders: Vec<u64> = Vec::new();
    let mut resting_bid: Option<(u64, u64)> = None; // (cl_ord_id, price)
    let mut resting_ask: Option<(u64, u64)> = None; // (cl_ord_id, price)
    let mut q_learner: Option<QLearningTable> = None;
    let mut prev_q_step: Option<(usize, usize, i64)> = None; // (state, action, mtm_pnl_fp)
    let mut bandit_learner: Option<ContextualBandit> = None;
    let mut prev_bandit_step: Option<(usize, usize, i64)> = None; // (ctx, arm, mtm_pnl_fp)
    let mut rng_state: u64 =
        0x9E37_79B9_7F4A_7C15 ^ (strat_idx as u64 + 1).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let mut tick: u64 = 0;

    while running.load(Ordering::Relaxed) {
        tick += 1;

        match kind {
            StrategyKind::MarketMaker {
                half_spread,
                lot_size,
            } => {
                // Cancel previous resting quotes before refreshing
                for old_id in active_orders.drain(..) {
                    let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                }

                let mid = state.book.read().unwrap().mid_price();
                let pos = state.position.load(Ordering::Relaxed);
                // Skew quotes by 1 tick per 100 shares of inventory to mean-revert position
                let skew = (pos / 100) * 5;
                let skewed_mid = (mid as i64 - skew).max(10_000) as u64;

                let bid_px = skewed_mid.saturating_sub(half_spread);
                let ask_px = skewed_mid + half_spread;

                let bid_id = state.next_id();
                let ask_id = state.next_id();
                let _ = tx.send(OuchInbound::EnterOrder {
                    cl_ord_id: bid_id,
                    side: Side::Buy,
                    shares: lot_size,
                    price: bid_px,
                });
                let _ = tx.send(OuchInbound::EnterOrder {
                    cl_ord_id: ask_id,
                    side: Side::Sell,
                    shares: lot_size,
                    price: ask_px,
                });
                active_orders.push(bid_id);
                active_orders.push(ask_id);

                thread::sleep(Duration::from_millis(4));
            }

            StrategyKind::MeanReversion {
                fair_value,
                threshold,
            } => {
                for old_id in active_orders.drain(..) {
                    let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                }

                let (best_bid, best_ask) = {
                    let b = state.book.read().unwrap();
                    (b.best_bid(), b.best_ask())
                };

                if let Some((ask_px, _)) = best_ask
                    && ask_px + threshold < fair_value
                {
                    let id = state.next_id();
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: id,
                        side: Side::Buy,
                        shares: 50,
                        price: ask_px,
                    });
                    active_orders.push(id);
                } else if let Some((bid_px, _)) = best_bid
                    && bid_px > fair_value + threshold
                {
                    let id = state.next_id();
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: id,
                        side: Side::Sell,
                        shares: 50,
                        price: bid_px,
                    });
                    active_orders.push(id);
                }

                thread::sleep(Duration::from_millis(6));
            }

            StrategyKind::MomentumSniper { lot_size } => {
                for old_id in active_orders.drain(..) {
                    let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                }

                let (buy_vol, sell_vol, best_bid, best_ask) = {
                    let b = state.book.read().unwrap();
                    (
                        b.recent_buy_exec_vol,
                        b.recent_sell_exec_vol,
                        b.best_bid(),
                        b.best_ask(),
                    )
                };

                if buy_vol >= sell_vol
                    && let Some((ask_px, _)) = best_ask
                {
                    let id = state.next_id();
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: id,
                        side: Side::Buy,
                        shares: lot_size,
                        price: ask_px + 10,
                    });
                    active_orders.push(id);
                } else if let Some((bid_px, _)) = best_bid {
                    let id = state.next_id();
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: id,
                        side: Side::Sell,
                        shares: lot_size,
                        price: bid_px.saturating_sub(10),
                    });
                    active_orders.push(id);
                }

                thread::sleep(Duration::from_millis(7));
            }

            StrategyKind::LiquidityTaker { lot_size } => {
                for old_id in active_orders.drain(..) {
                    let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                }

                let (best_bid, best_ask) = {
                    let b = state.book.read().unwrap();
                    (b.best_bid(), b.best_ask())
                };
                // Alternate aggressive buy/sell pings to create realistic tape flow
                let wobble = ((tick + strat_idx as u64) % 5) * 15;
                let id = state.next_id();
                if tick.is_multiple_of(2) {
                    let px = best_ask
                        .map(|(a, _)| a + wobble)
                        .unwrap_or(100 * PRICE_SCALE + 25);
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: id,
                        side: Side::Buy,
                        shares: lot_size,
                        price: px,
                    });
                } else {
                    let px = best_bid
                        .map(|(b, _)| b.saturating_sub(wobble))
                        .unwrap_or(100 * PRICE_SCALE - 25);
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: id,
                        side: Side::Sell,
                        shares: lot_size,
                        price: px,
                    });
                }
                active_orders.push(id);

                thread::sleep(Duration::from_millis(5));
            }

            StrategyKind::AvellanedaStoikovMM {
                risk_aversion_bps,
                base_half_spread,
                lot_size,
                max_inventory,
            } => {
                let (fair_px, vol, toxicity, best_bid, best_ask) = {
                    let b = state.book.read().unwrap();
                    (
                        b.smart_fair_value(),
                        b.ewma_volatility_fp(),
                        b.order_flow_toxicity(),
                        b.best_bid(),
                        b.best_ask(),
                    )
                };
                let pos = state.position.load(Ordering::Relaxed);

                // Avellaneda-Stoikov reservation price: r(s, q) = s_fair - q * gamma * sigma
                let gamma = (risk_aversion_bps as f64) / 10_000.0;
                let sigma = vol.clamp(3.0, 50.0);
                let inv_skew = (pos as f64) * gamma * sigma * 2.0;
                let reservation_px = (fair_px as f64 - inv_skew).max(10_000.0);

                // Adaptive half-spread scales with realized volatility and adverse selection (toxicity)
                let dynamic_half_spread =
                    (base_half_spread as f64 + 0.45 * sigma + 9.0 * toxicity.abs()).max(5.0);

                // Asymmetric toxicity guard: pull back the side exposed to informed flow
                let (bid_tox_adj, ask_tox_adj) = if toxicity > 0.15 {
                    (-2.0 * toxicity, 8.0 * toxicity)
                } else if toxicity < -0.15 {
                    (8.0 * (-toxicity), 2.0 * toxicity)
                } else {
                    (0.0, 0.0)
                };

                let mut target_bid = (reservation_px - dynamic_half_spread - bid_tox_adj)
                    .round()
                    .max(10_000.0) as u64;
                let mut target_ask = (reservation_px + dynamic_half_spread + ask_tox_adj)
                    .round()
                    .max(10_001.0) as u64;

                // Keep passive MM quotes from unintentionally crossing BBO
                if let Some((a_px, _)) = best_ask {
                    target_bid = target_bid.min(a_px.saturating_sub(1));
                }
                if let Some((b_px, _)) = best_bid {
                    target_ask = target_ask.max(b_px + 1);
                }
                if target_ask <= target_bid {
                    target_ask = target_bid + 2;
                }

                let force_refresh = tick.is_multiple_of(6);

                // Bid quote management with queue-priority preservation and inventory cap
                if pos < max_inventory {
                    let need_new_bid = match resting_bid {
                        Some((_, old_px)) => force_refresh || old_px.abs_diff(target_bid) > 2,
                        None => true,
                    };
                    if need_new_bid {
                        if let Some((old_id, _)) = resting_bid.take() {
                            let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                        }
                        let inv_ratio =
                            ((max_inventory - pos) as f64 / (max_inventory as f64)).clamp(0.25, 1.5);
                        let bid_shares = ((lot_size as f64) * inv_ratio).round().max(10.0) as u32;
                        let bid_id = state.next_id();
                        let _ = tx.send(OuchInbound::EnterOrder {
                            cl_ord_id: bid_id,
                            side: Side::Buy,
                            shares: bid_shares,
                            price: target_bid,
                        });
                        resting_bid = Some((bid_id, target_bid));
                    }
                } else if let Some((old_id, _)) = resting_bid.take() {
                    let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                }

                // Ask quote management with queue-priority preservation and inventory cap
                if pos > -max_inventory {
                    let need_new_ask = match resting_ask {
                        Some((_, old_px)) => force_refresh || old_px.abs_diff(target_ask) > 2,
                        None => true,
                    };
                    if need_new_ask {
                        if let Some((old_id, _)) = resting_ask.take() {
                            let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                        }
                        let inv_ratio =
                            ((max_inventory + pos) as f64 / (max_inventory as f64)).clamp(0.25, 1.5);
                        let ask_shares = ((lot_size as f64) * inv_ratio).round().max(10.0) as u32;
                        let ask_id = state.next_id();
                        let _ = tx.send(OuchInbound::EnterOrder {
                            cl_ord_id: ask_id,
                            side: Side::Sell,
                            shares: ask_shares,
                            price: target_ask,
                        });
                        resting_ask = Some((ask_id, target_ask));
                    }
                } else if let Some((old_id, _)) = resting_ask.take() {
                    let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                }

                thread::sleep(Duration::from_millis(3));
            }

            StrategyKind::MicropriceImbalanceAlpha {
                edge_threshold,
                lot_size,
                max_inventory,
            } => {
                // Cancel any short-lived passive join orders from the prior cycle
                for old_id in active_orders.drain(..) {
                    let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                }

                let (smart_fv, obi, toxicity, best_bid, best_ask) = {
                    let b = state.book.read().unwrap();
                    (
                        b.smart_fair_value(),
                        b.order_book_imbalance(5),
                        b.order_flow_toxicity(),
                        b.best_bid(),
                        b.best_ask(),
                    )
                };
                let pos = state.position.load(Ordering::Relaxed);

                // Inventory-adjusted fair value tilts toward unwinding large positions
                let inv_penalty = (pos / 50) * 2;
                let adj_fv = (smart_fv as i64 - inv_penalty).max(10_000) as u64;

                let mut sniped = false;

                // 1. Aggressive upward microprice + OBI breakout snipe
                if pos < max_inventory
                    && let Some((ask_px, ask_qty)) = best_ask
                    && adj_fv >= ask_px + edge_threshold
                    && obi > 0.08
                    && toxicity >= -0.25
                {
                    let edge = (adj_fv - ask_px).min(40);
                    let scale = 1.0 + (edge as f64 / 30.0);
                    let desired = ((lot_size as f64) * scale).round() as u32;
                    let room = (max_inventory - pos).max(0) as u32;
                    let shares = desired.min(ask_qty.max(lot_size)).min(room);
                    if shares > 0 {
                        let _ = tx.send(OuchInbound::EnterOrder {
                            cl_ord_id: state.next_id(),
                            side: Side::Buy,
                            shares,
                            price: ask_px + 2,
                        });
                        sniped = true;
                    }
                }

                // 2. Aggressive downward microprice + OBI breakdown snipe
                if !sniped
                    && pos > -max_inventory
                    && let Some((bid_px, bid_qty)) = best_bid
                    && adj_fv + edge_threshold <= bid_px
                    && obi < -0.08
                    && toxicity <= 0.25
                {
                    let edge = (bid_px - adj_fv).min(40);
                    let scale = 1.0 + (edge as f64 / 30.0);
                    let desired = ((lot_size as f64) * scale).round() as u32;
                    let room = (max_inventory + pos).max(0) as u32;
                    let shares = desired.min(bid_qty.max(lot_size)).min(room);
                    if shares > 0 {
                        let _ = tx.send(OuchInbound::EnterOrder {
                            cl_ord_id: state.next_id(),
                            side: Side::Sell,
                            shares,
                            price: bid_px.saturating_sub(2),
                        });
                        sniped = true;
                    }
                }

                // 3. Opportunistic passive BBO pegging when strong OBI or inventory overhang exists
                if !sniped {
                    if (obi > 0.25 || pos < -(lot_size as i64))
                        && pos < max_inventory
                        && let Some((bid_px, _)) = best_bid
                    {
                        let peg_px = match best_ask {
                            Some((ask_px, _)) if ask_px > bid_px + 2 => bid_px + 1,
                            _ => bid_px,
                        };
                        let id = state.next_id();
                        let _ = tx.send(OuchInbound::EnterOrder {
                            cl_ord_id: id,
                            side: Side::Buy,
                            shares: lot_size,
                            price: peg_px,
                        });
                        active_orders.push(id);
                    } else if (obi < -0.25 || pos > (lot_size as i64))
                        && pos > -max_inventory
                        && let Some((ask_px, _)) = best_ask
                    {
                        let peg_px = match best_bid {
                            Some((bid_px, _)) if ask_px > bid_px + 2 => ask_px - 1,
                            _ => ask_px,
                        };
                        let id = state.next_id();
                        let _ = tx.send(OuchInbound::EnterOrder {
                            cl_ord_id: id,
                            side: Side::Sell,
                            shares: lot_size,
                            price: peg_px,
                        });
                        active_orders.push(id);
                    }
                }

                thread::sleep(Duration::from_millis(4));
            }

            StrategyKind::TabularQLearningMM {
                lot_size,
                max_inventory,
                learning_rate_bps,
                discount_bps,
            } => {
                // Lazy-init Q-learning table by loading persisted weights from disk (or priors on first run)
                let q_agent = q_learner.get_or_insert_with(|| {
                    let path = QLearningTable::default_weights_path();
                    let table = QLearningTable::load_or_init(&path);
                    state.rl_q_updates.store(table.updates, Ordering::Relaxed);
                    table
                });
                let alpha = (learning_rate_bps as f64 / 10_000.0).clamp(0.01, 0.99);
                let gamma = (discount_bps as f64 / 10_000.0).clamp(0.0, 0.999);

                let (fair_px, obi, vol, toxicity, best_bid, best_ask) = {
                    let b = state.book.read().unwrap();
                    (
                        b.smart_fair_value(),
                        b.order_book_imbalance(5),
                        b.ewma_volatility_fp(),
                        b.order_flow_toxicity(),
                        b.best_bid(),
                        b.best_ask(),
                    )
                };
                let snap = state.pnl_snapshot();
                let pos = snap.position;
                let cur_state = QLearningTable::encode_state(pos, obi, vol, toxicity);

                // Online Bellman TD(0) update from previous transition (s, a, r, s')
                if let Some((prev_state, prev_action, prev_pnl_fp)) = prev_q_step {
                    let delta_pnl_dollars =
                        (snap.mtm_pnl_fp - prev_pnl_fp) as f64 / (PRICE_SCALE as f64);
                    // Quadratic inventory penalty encourages mean-reverting inventory control
                    let inv_norm = (pos as f64) / 150.0;
                    let reward = delta_pnl_dollars - 0.08 * (inv_norm * inv_norm);
                    q_agent.update(prev_state, prev_action, reward, cur_state, alpha, gamma);
                    state.rl_q_updates.store(q_agent.updates, Ordering::Relaxed);
                    // Periodically checkpoint weights to disk so progress is preserved even on abrupt stop
                    if q_agent.updates.is_multiple_of(25) {
                        let _ = q_agent.save_to_file(&QLearningTable::default_weights_path());
                    }
                }

                // Decaying epsilon-greedy exploration driven by cumulative persisted updates across runs
                let epsilon = (0.18 / (1.0 + (q_agent.updates as f64) * 0.008)).max(0.03);
                let action = q_agent.select_action(cur_state, epsilon, &mut rng_state);
                prev_q_step = Some((cur_state, action, snap.mtm_pnl_fp));

                let (bid_offset, ask_offset) = QLearningTable::action_offsets(action, pos);
                let inv_skew = (pos / 80) * 3;
                let center_px = (fair_px as i64 - inv_skew).max(10_000) as u64;

                let mut target_bid = center_px.saturating_sub(bid_offset).max(10_000);
                let mut target_ask = (center_px + ask_offset).max(10_001);

                if let Some((a_px, _)) = best_ask {
                    target_bid = target_bid.min(a_px.saturating_sub(1));
                }
                if let Some((b_px, _)) = best_bid {
                    target_ask = target_ask.max(b_px + 1);
                }
                if target_ask <= target_bid {
                    target_ask = target_bid + 2;
                }

                let force_refresh = tick.is_multiple_of(5);

                if pos < max_inventory {
                    let need_new_bid = match resting_bid {
                        Some((_, old_px)) => force_refresh || old_px.abs_diff(target_bid) > 2,
                        None => true,
                    };
                    if need_new_bid {
                        if let Some((old_id, _)) = resting_bid.take() {
                            let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                        }
                        let bid_id = state.next_id();
                        let _ = tx.send(OuchInbound::EnterOrder {
                            cl_ord_id: bid_id,
                            side: Side::Buy,
                            shares: lot_size,
                            price: target_bid,
                        });
                        resting_bid = Some((bid_id, target_bid));
                    }
                } else if let Some((old_id, _)) = resting_bid.take() {
                    let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                }

                if pos > -max_inventory {
                    let need_new_ask = match resting_ask {
                        Some((_, old_px)) => force_refresh || old_px.abs_diff(target_ask) > 2,
                        None => true,
                    };
                    if need_new_ask {
                        if let Some((old_id, _)) = resting_ask.take() {
                            let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                        }
                        let ask_id = state.next_id();
                        let _ = tx.send(OuchInbound::EnterOrder {
                            cl_ord_id: ask_id,
                            side: Side::Sell,
                            shares: lot_size,
                            price: target_ask,
                        });
                        resting_ask = Some((ask_id, target_ask));
                    }
                } else if let Some((old_id, _)) = resting_ask.take() {
                    let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                }

                thread::sleep(Duration::from_millis(4));
            }

            StrategyKind::ContextualBanditTaker {
                lot_size,
                max_inventory,
                exploration_bps,
            } => {
                for old_id in active_orders.drain(..) {
                    let _ = tx.send(OuchInbound::CancelOrder { cl_ord_id: old_id });
                }

                // Lazy-init Contextual Bandit by loading persisted weights from disk (or priors on first run)
                let bandit = bandit_learner.get_or_insert_with(|| {
                    let path = ContextualBandit::default_weights_path();
                    let b = ContextualBandit::load_or_init(&path);
                    state.rl_bandit_updates.store(b.updates, Ordering::Relaxed);
                    b
                });
                let exploration_c = (exploration_bps as f64 / 10_000.0).max(0.01);

                let (smart_fv, obi, toxicity, best_bid, best_ask) = {
                    let b = state.book.read().unwrap();
                    (
                        b.smart_fair_value(),
                        b.order_book_imbalance(5),
                        b.order_flow_toxicity(),
                        b.best_bid(),
                        b.best_ask(),
                    )
                };
                let snap = state.pnl_snapshot();
                let pos = snap.position;

                // Online contextual bandit update from previous step's realized reward
                if let Some((prev_ctx, prev_arm, prev_pnl_fp)) = prev_bandit_step {
                    let delta_pnl_dollars =
                        (snap.mtm_pnl_fp - prev_pnl_fp) as f64 / (PRICE_SCALE as f64);
                    let inv_norm = (pos as f64) / 200.0;
                    let reward = delta_pnl_dollars - 0.05 * (inv_norm * inv_norm);
                    bandit.update(prev_ctx, prev_arm, reward);
                    state.rl_bandit_updates.store(bandit.updates, Ordering::Relaxed);
                    if bandit.updates.is_multiple_of(25) {
                        let _ = bandit.save_to_file(&ContextualBandit::default_weights_path());
                    }
                }

                let ctx = ContextualBandit::encode_context(obi, toxicity);
                let arm = bandit.select_arm(ctx, pos, max_inventory, exploration_c);
                prev_bandit_step = Some((ctx, arm, snap.mtm_pnl_fp));

                match arm {
                    // Arm 1: Snipe Ask (Aggressive Buy) when microprice/OBI supports it
                    1 => {
                        if pos < max_inventory
                            && let Some((ask_px, _)) = best_ask
                            && smart_fv + 4 >= ask_px
                        {
                            let id = state.next_id();
                            let _ = tx.send(OuchInbound::EnterOrder {
                                cl_ord_id: id,
                                side: Side::Buy,
                                shares: lot_size,
                                price: ask_px + 2,
                            });
                            active_orders.push(id);
                        }
                    }
                    // Arm 2: Hit Bid (Aggressive Sell) when microprice/OBI supports it
                    2 => {
                        if pos > -max_inventory
                            && let Some((bid_px, _)) = best_bid
                            && smart_fv <= bid_px + 4
                        {
                            let id = state.next_id();
                            let _ = tx.send(OuchInbound::EnterOrder {
                                cl_ord_id: id,
                                side: Side::Sell,
                                shares: lot_size,
                                price: bid_px.saturating_sub(2),
                            });
                            active_orders.push(id);
                        }
                    }
                    // Arm 3: Passive BBO Peg on Microprice-Favored or Inventory-Unwinding Side
                    3 => {
                        if (obi >= 0.0 || pos < -100)
                            && pos < max_inventory
                            && let Some((bid_px, _)) = best_bid
                        {
                            let peg_px = match best_ask {
                                Some((ask_px, _)) if ask_px > bid_px + 2 => bid_px + 1,
                                _ => bid_px,
                            };
                            let id = state.next_id();
                            let _ = tx.send(OuchInbound::EnterOrder {
                                cl_ord_id: id,
                                side: Side::Buy,
                                shares: lot_size,
                                price: peg_px,
                            });
                            active_orders.push(id);
                        } else if pos > -max_inventory
                            && let Some((ask_px, _)) = best_ask
                        {
                            let peg_px = match best_bid {
                                Some((bid_px, _)) if ask_px > bid_px + 2 => ask_px - 1,
                                _ => ask_px,
                            };
                            let id = state.next_id();
                            let _ = tx.send(OuchInbound::EnterOrder {
                                cl_ord_id: id,
                                side: Side::Sell,
                                shares: lot_size,
                                price: peg_px,
                            });
                            active_orders.push(id);
                        }
                    }
                    // Arm 0: Hold / No-Op
                    _ => {}
                }

                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    // Flush latest learned RL weights to disk on clean strategy shutdown
    if let Some(ref q_agent) = q_learner {
        let _ = q_agent.save_to_file(&QLearningTable::default_weights_path());
    }
    if let Some(ref bandit) = bandit_learner {
        let _ = bandit.save_to_file(&ContextualBandit::default_weights_path());
    }

    let _ = fmt_price(0);
}

// ============================================================================
// Online Reinforcement Learning Primitives (Tabular Q-Learning + Contextual UCB1)
// ============================================================================

static RL_TMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Resolves the directory used to persist Bot 4's RL weights between simulation runs.
///
/// Honors `ITCH_OUCH_RL_WEIGHTS_DIR` if set; otherwise uses `itch-ouch-sim/rl_weights`
/// when invoked from the repository root or `rl_weights` when invoked from `itch-ouch-sim/`.
pub fn rl_weights_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ITCH_OUCH_RL_WEIGHTS_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    if Path::new("itch-ouch-sim/Cargo.toml").exists() {
        PathBuf::from("itch-ouch-sim/rl_weights")
    } else {
        PathBuf::from("rl_weights")
    }
}

/// Atomically writes `content` to `path` via a sibling temporary file + `fs::rename`.
fn atomic_write_text(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let seq = RL_TMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = path.with_extension(format!("tmp.{}.{}", std::process::id(), seq));
    fs::write(&tmp_path, content)?;
    fs::rename(&tmp_path, path)
}

/// Fast xorshift64 PRNG for lock-free epsilon-greedy exploration
#[inline]
fn next_u64_rng(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// Tabular Q-Learning agent over 45 discrete microstructure states and 6 quoting actions.
///
/// State factorization (`5 * 3 * 3 = 45`):
/// - Inventory bucket (`0..5`): Deep Short, Mild Short, Neutral, Mild Long, Deep Long
/// - Order-Book Imbalance (`0..3`): Bearish (`< -0.15`), Balanced, Bullish (`> +0.15`)
/// - Volatility / Toxicity (`0..3`): Calm, Moderate, Turbulent/Toxic
#[derive(Clone, Debug)]
pub struct QLearningTable {
    pub q: [[f64; Self::NUM_ACTIONS]; Self::NUM_STATES],
    pub updates: u64,
}

impl QLearningTable {
    pub const NUM_STATES: usize = 45;
    pub const NUM_ACTIONS: usize = 6;
    pub const WEIGHTS_FILE_NAME: &'static str = "bot4_q_table.weights";

    pub fn default_weights_path() -> PathBuf {
        rl_weights_dir().join(Self::WEIGHTS_FILE_NAME)
    }

    pub fn new_with_priors() -> Self {
        let mut q = [[0.0; Self::NUM_ACTIONS]; Self::NUM_STATES];
        for inv_idx in 0..5 {
            for obi_idx in 0..3 {
                for vol_idx in 0..3 {
                    let s = inv_idx * 9 + obi_idx * 3 + vol_idx;
                    // Action 0: Tight Symmetric (9, 9) — good when calm and neutral inventory
                    q[s][0] = if vol_idx == 0 && inv_idx == 2 { 0.25 } else { 0.05 };
                    // Action 1: Moderate Symmetric (16, 16) — balanced baseline
                    q[s][1] = 0.20;
                    // Action 2: Wide Defensive (28, 28) — best when vol/toxicity is high
                    q[s][2] = if vol_idx == 2 { 0.35 } else { 0.10 };
                    // Action 3: Skew Bid / Accumulate Long (6, 24) — favored when short or bullish OBI
                    q[s][3] = if inv_idx <= 1 || obi_idx == 2 { 0.30 } else { -0.05 };
                    // Action 4: Skew Ask / Accumulate Short (24, 6) — favored when long or bearish OBI
                    q[s][4] = if inv_idx >= 3 || obi_idx == 0 { 0.30 } else { -0.05 };
                    // Action 5: Inventory-Flattening Peg — strongly favored at extreme inventory
                    q[s][5] = if inv_idx == 0 || inv_idx == 4 { 0.40 } else { 0.12 };
                }
            }
        }
        Self { q, updates: 0 }
    }

    /// Loads persisted Q-Learning weights from `path`, or initializes with domain priors and saves them.
    pub fn load_or_init(path: &Path) -> Self {
        match Self::load_from_file(path) {
            Ok(table) => table,
            Err(_) => {
                let table = Self::new_with_priors();
                let _ = table.save_to_file(path);
                table
            }
        }
    }

    /// Serializes the Q-table and cumulative update count to human-readable text.
    pub fn to_weights_string(&self) -> String {
        let mut out = String::with_capacity(2048);
        out.push_str("# ITCH-OUCH-SIM TabularQLearningMM Weights v1\n");
        out.push_str(&format!("updates={}\n", self.updates));
        for (s, row) in self.q.iter().enumerate() {
            out.push_str(&format!(
                "state {} {:.8} {:.8} {:.8} {:.8} {:.8} {:.8}\n",
                s, row[0], row[1], row[2], row[3], row[4], row[5]
            ));
        }
        out
    }

    /// Parses Q-table weights and cumulative update count from text.
    pub fn from_weights_str(content: &str) -> std::io::Result<Self> {
        let mut table = Self::new_with_priors();
        let mut parsed_states = 0usize;

        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("updates=") {
                table.updates = rest
                    .trim()
                    .parse::<u64>()
                    .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() == 2 + Self::NUM_ACTIONS && parts[0] == "state" {
                let s = parts[1]
                    .parse::<usize>()
                    .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
                if s >= Self::NUM_STATES {
                    return Err(Error::new(ErrorKind::InvalidData, "state index out of range"));
                }
                for a in 0..Self::NUM_ACTIONS {
                    let val = parts[2 + a]
                        .parse::<f64>()
                        .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
                    if !val.is_finite() {
                        return Err(Error::new(ErrorKind::InvalidData, "non-finite Q value"));
                    }
                    table.q[s][a] = val;
                }
                parsed_states += 1;
            }
        }

        if parsed_states != Self::NUM_STATES {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "incomplete Q-table state rows",
            ));
        }
        Ok(table)
    }

    pub fn save_to_file(&self, path: &Path) -> std::io::Result<()> {
        atomic_write_text(path, &self.to_weights_string())
    }

    pub fn load_from_file(path: &Path) -> std::io::Result<Self> {
        let content = fs::read_to_string(path)?;
        Self::from_weights_str(&content)
    }

    pub fn encode_state(pos: i64, obi: f64, vol: f64, toxicity: f64) -> usize {
        let inv_idx = if pos < -250 {
            0
        } else if pos < -50 {
            1
        } else if pos <= 50 {
            2
        } else if pos <= 250 {
            3
        } else {
            4
        };

        let obi_idx = if obi < -0.15 {
            0
        } else if obi > 0.15 {
            2
        } else {
            1
        };

        let tox_abs = toxicity.abs();
        let vol_idx = if vol >= 18.0 || tox_abs >= 0.50 {
            2
        } else if vol < 8.0 && tox_abs < 0.25 {
            0
        } else {
            1
        };

        inv_idx * 9 + obi_idx * 3 + vol_idx
    }

    pub fn select_action(&self, state: usize, epsilon: f64, rng_state: &mut u64) -> usize {
        let s = state.min(Self::NUM_STATES - 1);
        let rand_01 = (next_u64_rng(rng_state) >> 11) as f64 / ((1u64 << 53) as f64);
        if rand_01 < epsilon {
            (next_u64_rng(rng_state) as usize) % Self::NUM_ACTIONS
        } else {
            let mut best_a = 0;
            let mut best_q = self.q[s][0];
            for (a, &val) in self.q[s].iter().enumerate().skip(1) {
                if val > best_q {
                    best_q = val;
                    best_a = a;
                }
            }
            best_a
        }
    }

    pub fn update(
        &mut self,
        prev_state: usize,
        action: usize,
        reward: f64,
        next_state: usize,
        alpha: f64,
        gamma: f64,
    ) -> f64 {
        let s = prev_state.min(Self::NUM_STATES - 1);
        let a = action.min(Self::NUM_ACTIONS - 1);
        let ns = next_state.min(Self::NUM_STATES - 1);

        let max_next_q = self.q[ns]
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);
        let td_target = reward + gamma * max_next_q;
        let td_error = td_target - self.q[s][a];
        self.q[s][a] += alpha * td_error;
        self.updates += 1;
        td_error
    }

    /// Maps discrete action `0..6` to `(bid_half_spread, ask_half_spread)` in fixed-point ticks.
    pub fn action_offsets(action: usize, pos: i64) -> (u64, u64) {
        match action {
            0 => (9, 9),   // Tight Symmetric
            1 => (16, 16), // Moderate Symmetric
            2 => (28, 28), // Wide Defensive
            3 => (6, 24),  // Skew Bid (accumulate long / cover short)
            4 => (24, 6),  // Skew Ask (accumulate short / unwind long)
            _ => {
                // Action 5: Inventory-flattening peg
                if pos > 40 {
                    (26, 5)
                } else if pos < -40 {
                    (5, 26)
                } else {
                    (12, 12)
                }
            }
        }
    }
}

/// Online Contextual UCB1 Bandit over 9 market regimes (`3 OBI × 3 Toxicity`) and 4 execution arms:
/// - Arm `0`: Hold / Wait
/// - Arm `1`: Snipe Ask (Aggressive Buy)
/// - Arm `2`: Hit Bid (Aggressive Sell)
/// - Arm `3`: Passive BBO Peg on Microprice Side
#[derive(Clone, Debug)]
pub struct ContextualBandit {
    pub mean_reward: [[f64; Self::NUM_ARMS]; Self::NUM_CONTEXTS],
    pub counts: [[u64; Self::NUM_ARMS]; Self::NUM_CONTEXTS],
    pub context_visits: [u64; Self::NUM_CONTEXTS],
    pub updates: u64,
}

impl ContextualBandit {
    pub const NUM_CONTEXTS: usize = 9;
    pub const NUM_ARMS: usize = 4;
    pub const WEIGHTS_FILE_NAME: &'static str = "bot4_bandit.weights";

    pub fn default_weights_path() -> PathBuf {
        rl_weights_dir().join(Self::WEIGHTS_FILE_NAME)
    }

    pub fn new_with_priors() -> Self {
        let mut mean_reward = [[0.0; Self::NUM_ARMS]; Self::NUM_CONTEXTS];
        let counts = [[1u64; Self::NUM_ARMS]; Self::NUM_CONTEXTS];
        let context_visits = [4u64; Self::NUM_CONTEXTS];

        for obi_idx in 0..3 {
            for tox_idx in 0..3 {
                let ctx = obi_idx * 3 + tox_idx;
                // Arm 0: Hold
                mean_reward[ctx][0] = 0.05;
                // Arm 1: Snipe Ask — prior boost when OBI & flow are bullish (obi_idx == 2, tox_idx >= 1)
                mean_reward[ctx][1] = if obi_idx == 2 && tox_idx >= 1 {
                    0.35
                } else {
                    -0.10
                };
                // Arm 2: Hit Bid — prior boost when OBI & flow are bearish (obi_idx == 0, tox_idx <= 1)
                mean_reward[ctx][2] = if obi_idx == 0 && tox_idx <= 1 {
                    0.35
                } else {
                    -0.10
                };
                // Arm 3: Passive BBO Peg
                mean_reward[ctx][3] = 0.18;
            }
        }

        Self {
            mean_reward,
            counts,
            context_visits,
            updates: 0,
        }
    }

    /// Loads persisted Contextual Bandit weights from `path`, or initializes with priors and saves them.
    pub fn load_or_init(path: &Path) -> Self {
        match Self::load_from_file(path) {
            Ok(bandit) => bandit,
            Err(_) => {
                let bandit = Self::new_with_priors();
                let _ = bandit.save_to_file(path);
                bandit
            }
        }
    }

    /// Serializes Contextual Bandit counts, visit totals, and arm rewards to human-readable text.
    pub fn to_weights_string(&self) -> String {
        let mut out = String::with_capacity(1024);
        out.push_str("# ITCH-OUCH-SIM ContextualBanditTaker Weights v1\n");
        out.push_str(&format!("updates={}\n", self.updates));
        for ctx in 0..Self::NUM_CONTEXTS {
            let c = &self.counts[ctx];
            let r = &self.mean_reward[ctx];
            out.push_str(&format!(
                "context {} visits {} counts {} {} {} {} rewards {:.8} {:.8} {:.8} {:.8}\n",
                ctx,
                self.context_visits[ctx],
                c[0],
                c[1],
                c[2],
                c[3],
                r[0],
                r[1],
                r[2],
                r[3]
            ));
        }
        out
    }

    /// Parses Contextual Bandit state from text.
    pub fn from_weights_str(content: &str) -> std::io::Result<Self> {
        let mut bandit = Self::new_with_priors();
        let mut parsed_contexts = 0usize;

        for raw_line in content.lines() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("updates=") {
                bandit.updates = rest
                    .trim()
                    .parse::<u64>()
                    .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            // Format: context <ctx> visits <v> counts <c0> <c1> <c2> <c3> rewards <r0> <r1> <r2> <r3>
            if parts.len() == 14
                && parts[0] == "context"
                && parts[2] == "visits"
                && parts[4] == "counts"
                && parts[9] == "rewards"
            {
                let ctx = parts[1]
                    .parse::<usize>()
                    .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
                if ctx >= Self::NUM_CONTEXTS {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "context index out of range",
                    ));
                }
                bandit.context_visits[ctx] = parts[3]
                    .parse::<u64>()
                    .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
                for arm in 0..Self::NUM_ARMS {
                    bandit.counts[ctx][arm] = parts[5 + arm]
                        .parse::<u64>()
                        .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
                    let val = parts[10 + arm]
                        .parse::<f64>()
                        .map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
                    if !val.is_finite() {
                        return Err(Error::new(ErrorKind::InvalidData, "non-finite arm reward"));
                    }
                    bandit.mean_reward[ctx][arm] = val;
                }
                parsed_contexts += 1;
            }
        }

        if parsed_contexts != Self::NUM_CONTEXTS {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "incomplete ContextualBandit rows",
            ));
        }
        Ok(bandit)
    }

    pub fn save_to_file(&self, path: &Path) -> std::io::Result<()> {
        atomic_write_text(path, &self.to_weights_string())
    }

    pub fn load_from_file(path: &Path) -> std::io::Result<Self> {
        let content = fs::read_to_string(path)?;
        Self::from_weights_str(&content)
    }

    pub fn encode_context(obi: f64, toxicity: f64) -> usize {
        let obi_idx = if obi < -0.12 {
            0
        } else if obi > 0.12 {
            2
        } else {
            1
        };
        let tox_idx = if toxicity < -0.15 {
            0
        } else if toxicity > 0.15 {
            2
        } else {
            1
        };
        obi_idx * 3 + tox_idx
    }

    pub fn select_arm(&self, ctx: usize, pos: i64, max_inventory: i64, exploration_c: f64) -> usize {
        let c = ctx.min(Self::NUM_CONTEXTS - 1);
        let total_n = (self.context_visits[c].max(1) as f64).ln();

        let mut best_arm = 0;
        let mut best_score = f64::NEG_INFINITY;

        for arm in 0..Self::NUM_ARMS {
            // Disallow inventory-increasing aggressive arms at inventory cap
            if arm == 1 && pos >= max_inventory {
                continue;
            }
            if arm == 2 && pos <= -max_inventory {
                continue;
            }
            let n = self.counts[c][arm].max(1) as f64;
            let ucb_bonus = exploration_c * (total_n / n).sqrt();
            // Mild inventory-unwinding preference
            let inv_bonus = match arm {
                1 if pos < -100 => 0.10,
                2 if pos > 100 => 0.10,
                _ => 0.0,
            };
            let score = self.mean_reward[c][arm] + ucb_bonus + inv_bonus;
            if score > best_score {
                best_score = score;
                best_arm = arm;
            }
        }

        best_arm
    }

    pub fn update(&mut self, ctx: usize, arm: usize, reward: f64) {
        let c = ctx.min(Self::NUM_CONTEXTS - 1);
        let a = arm.min(Self::NUM_ARMS - 1);
        self.counts[c][a] += 1;
        self.context_visits[c] += 1;
        self.updates += 1;
        // Exponentially recency-weighted mean update for non-stationary order book dynamics
        const BETA: f64 = 0.15;
        self.mean_reward[c][a] = (1.0 - BETA) * self.mean_reward[c][a] + BETA * reward;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shadow_book_microprice_and_obi() {
        let mut book = ShadowBook::default();
        // Bid: 300 shares @ $99.9900 (999_900), Ask: 100 shares @ $100.0100 (1_000_100)
        book.apply(ItchMessage::OrderAdded {
            seq_num: 1,
            order_ref: 10,
            side: Side::Buy,
            shares: 300,
            price: 999_900,
        });
        book.apply(ItchMessage::OrderAdded {
            seq_num: 2,
            order_ref: 11,
            side: Side::Sell,
            shares: 100,
            price: 1_000_100,
        });

        assert_eq!(book.mid_price(), 1_000_000);
        // Microprice should be weighted 75% toward the ask: 999_900 * 0.25 + 1_000_100 * 0.75 = 1_000_050
        assert_eq!(book.micro_price(), 1_000_050);
        // Order book imbalance: (300 - 100) / (300 + 100) = +0.5
        let obi = book.order_book_imbalance(5);
        assert!((obi - 0.5).abs() < 1e-6);
        assert!(book.smart_fair_value() > book.mid_price());
    }

    #[test]
    fn test_order_flow_toxicity_and_bot3_profile() {
        let mut book = ShadowBook::default();
        book.apply(ItchMessage::OrderAdded {
            seq_num: 1,
            order_ref: 1,
            side: Side::Sell,
            shares: 200,
            price: 1_000_050,
        });
        book.apply(ItchMessage::OrderExecuted {
            seq_num: 2,
            order_ref: 1,
            side: Side::Sell,
            executed_shares: 120,
            match_price: 1_000_050,
        });

        // Aggressive buy lifted resting sell -> positive order flow toxicity
        assert!(book.order_flow_toxicity() > 0.9);

        let (name, strats) = default_strategies_for_bot(3);
        assert!(name.contains("Bot-3"));
        assert!(matches!(strats[0], StrategyKind::AvellanedaStoikovMM { .. }));
        assert!(matches!(
            strats[1],
            StrategyKind::MicropriceImbalanceAlpha { .. }
        ));
    }

    #[test]
    fn test_equal_fixed_starting_capital_and_buying_power() {
        let bot1 = BotState::new("Bot-1", 1);
        let bot2 = BotState::new("Bot-2", 2);
        let bot3 = BotState::new("Bot-3", 3);
        let bot4 = BotState::new("Bot-4", 4);

        // All 4 bots start with the exact same fixed capital ($100,000.00) and $0.00 net PnL
        assert_eq!(bot1.initial_capital_fp, INITIAL_BOT_CAPITAL_FP);
        assert_eq!(bot1.initial_capital_fp, bot2.initial_capital_fp);
        assert_eq!(bot2.initial_capital_fp, bot3.initial_capital_fp);
        assert_eq!(bot3.initial_capital_fp, bot4.initial_capital_fp);

        let snap = bot1.pnl_snapshot();
        assert_eq!(snap.cash_fp, INITIAL_BOT_CAPITAL_FP);
        assert_eq!(snap.equity_fp, INITIAL_BOT_CAPITAL_FP);
        assert_eq!(snap.mtm_pnl_fp, 0);

        // At $100.0000 per share (1_000_000 fp), $100,000.00 can buy at most 1,000 shares
        assert_eq!(
            bot1.affordable_shares(Side::Buy, 2_500, 100 * PRICE_SCALE),
            1_000
        );

        // If all cash is spent, further buy orders are clamped to 0 shares
        bot1.cash_fp.store(0, Ordering::Relaxed);
        bot1.position.store(1_000, Ordering::Relaxed);
        assert_eq!(
            bot1.affordable_shares(Side::Buy, 100, 100 * PRICE_SCALE),
            0
        );
    }

    #[test]
    fn test_rl_bot4_q_learning_and_contextual_bandit() {
        let (name, strats) = default_strategies_for_bot(4);
        assert!(name.contains("Bot-4"));
        assert!(matches!(strats[0], StrategyKind::TabularQLearningMM { .. }));
        assert!(matches!(
            strats[1],
            StrategyKind::ContextualBanditTaker { .. }
        ));

        // Verify Tabular Q-Learning Bellman update shifts greedy action toward high-reward action
        let mut q_table = QLearningTable::new_with_priors();
        let s = QLearningTable::encode_state(0, 0.0, 5.0, 0.0);
        let mut rng = 0x1234_5678_9abc_def0;
        for _ in 0..10 {
            q_table.update(s, 3, 5.0, s, 0.25, 0.90);
        }
        assert_eq!(q_table.select_action(s, 0.0, &mut rng), 3);

        // Verify Contextual UCB1 Bandit learns to prefer profitable arm in a given context
        let mut bandit = ContextualBandit::new_with_priors();
        let ctx = ContextualBandit::encode_context(0.30, 0.40);
        for _ in 0..10 {
            bandit.update(ctx, 1, 2.5);
            bandit.update(ctx, 2, -2.0);
        }
        assert_eq!(bandit.select_arm(ctx, 0, 600, 0.05), 1);
    }

    #[test]
    fn test_rl_weights_persistence_roundtrip() {
        let tmp_dir = std::env::temp_dir().join(format!(
            "itch_ouch_rl_test_{}",
             std::process::id()
        ));
        let q_path = tmp_dir.join("q_table.weights");
        let b_path = tmp_dir.join("bandit.weights");

        let mut q_table = QLearningTable::new_with_priors();
        let s = QLearningTable::encode_state(-120, 0.25, 12.0, 0.10);
        for _ in 0..15 {
            q_table.update(s, 4, 3.25, s, 0.20, 0.92);
        }
        q_table.save_to_file(&q_path).expect("save Q-table");

        let loaded_q = QLearningTable::load_from_file(&q_path).expect("load Q-table");
        assert_eq!(loaded_q.updates, 15);
        assert!((loaded_q.q[s][4] - q_table.q[s][4]).abs() < 1e-6);

        let mut bandit = ContextualBandit::new_with_priors();
        let ctx = ContextualBandit::encode_context(-0.30, -0.35);
        for _ in 0..12 {
            bandit.update(ctx, 2, 1.85);
        }
        bandit.save_to_file(&b_path).expect("save Bandit");

        let loaded_b = ContextualBandit::load_from_file(&b_path).expect("load Bandit");
        assert_eq!(loaded_b.updates, 12);
        assert_eq!(loaded_b.counts[ctx][2], bandit.counts[ctx][2]);
        assert_eq!(loaded_b.context_visits[ctx], bandit.context_visits[ctx]);
        assert!((loaded_b.mean_reward[ctx][2] - bandit.mean_reward[ctx][2]).abs() < 1e-6);

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}

