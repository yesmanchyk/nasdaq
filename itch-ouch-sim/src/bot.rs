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
use std::net::{Ipv4Addr, TcpStream};
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

/// Point-in-time PnL snapshot of a bot (all monetary values in fixed-point 1/10_000 dollars)
#[derive(Clone, Copy, Debug, Default)]
pub struct BotPnl {
    pub position: i64,
    pub cash_fp: i64,
    pub mark_fp: u64,
    pub mtm_pnl_fp: i64,
    pub fills: u64,
    pub acks: u64,
    pub cancels: u64,
}

/// Shared bot telemetry & position tracking across its concurrent strategies
pub struct BotState {
    pub name: String,
    pub book: RwLock<ShadowBook>,
    pub position: AtomicI64,
    /// Cash in fixed-point units (1/10_000 of a dollar)
    pub cash_fp: AtomicI64,
    pub fills_count: AtomicU64,
    pub acks_count: AtomicU64,
    pub cancels_count: AtomicU64,
    next_cl_ord_id: AtomicU64,
}

impl BotState {
    pub fn new(name: impl Into<String>, bot_id_prefix: u64) -> Self {
        Self {
            name: name.into(),
            book: RwLock::new(ShadowBook::default()),
            position: AtomicI64::new(0),
            cash_fp: AtomicI64::new(0),
            fills_count: AtomicU64::new(0),
            acks_count: AtomicU64::new(0),
            cancels_count: AtomicU64::new(0),
            next_cl_ord_id: AtomicU64::new(bot_id_prefix * 1_000_000 + 1),
        }
    }

    pub fn next_id(&self) -> u64 {
        self.next_cl_ord_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Point-in-time PnL / activity snapshot, marked to the bot's own shadow-book mid price.
    pub fn pnl_snapshot(&self) -> BotPnl {
        let mark_fp = self.book.read().unwrap().mid_price();
        let position = self.position.load(Ordering::Relaxed);
        let cash_fp = self.cash_fp.load(Ordering::Relaxed);
        BotPnl {
            position,
            cash_fp,
            mark_fp,
            mtm_pnl_fp: cash_fp + position * (mark_fp as i64),
            fills: self.fills_count.load(Ordering::Relaxed),
            acks: self.acks_count.load(Ordering::Relaxed),
            cancels: self.cancels_count.load(Ordering::Relaxed),
        }
    }

    pub fn summary(&self) -> String {
        let book = self.book.read().unwrap();
        let mid = book.mid_price();
        let pos = self.position.load(Ordering::Relaxed);
        let cash = self.cash_fp.load(Ordering::Relaxed);
        let mtm_pnl = cash + pos * (mid as i64);
        let pnl_dollars = mtm_pnl as f64 / (PRICE_SCALE as f64);

        format!(
            "[{}] PGM_ITCH: {:4} (RDATA:{}/NAK:{}) | Acks: {:4} | Cancels: {:4} | Fills: {:4} | Pos: {:+5} sh | MTM PnL: ${:+9.2}",
            self.name,
            book.itch_packets,
            book.pgm_rdata_recovered,
            book.pgm_naks_sent,
            self.acks_count.load(Ordering::Relaxed),
            self.cancels_count.load(Ordering::Relaxed),
            self.fills_count.load(Ordering::Relaxed),
            pos,
            pnl_dollars
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
        _ => (
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

    // 3. TCP OUCH Order Gateway Writer Thread (serializes orders from concurrent strategies)
    {
        let running = Arc::clone(&running);
        threads.push(thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match ouch_rx.recv_timeout(Duration::from_millis(5)) {
                    Ok(cmd) => {
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
                let (best_bid, best_ask) = {
                    let b = state.book.read().unwrap();
                    (b.best_bid(), b.best_ask())
                };

                if let Some((ask_px, _)) = best_ask
                    && ask_px + threshold < fair_value
                {
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: state.next_id(),
                        side: Side::Buy,
                        shares: 50,
                        price: ask_px,
                    });
                } else if let Some((bid_px, _)) = best_bid
                    && bid_px > fair_value + threshold
                {
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: state.next_id(),
                        side: Side::Sell,
                        shares: 50,
                        price: bid_px,
                    });
                }

                thread::sleep(Duration::from_millis(6));
            }

            StrategyKind::MomentumSniper { lot_size } => {
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
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: state.next_id(),
                        side: Side::Buy,
                        shares: lot_size,
                        price: ask_px + 10,
                    });
                } else if let Some((bid_px, _)) = best_bid {
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: state.next_id(),
                        side: Side::Sell,
                        shares: lot_size,
                        price: bid_px.saturating_sub(10),
                    });
                }

                thread::sleep(Duration::from_millis(7));
            }

            StrategyKind::LiquidityTaker { lot_size } => {
                let (best_bid, best_ask) = {
                    let b = state.book.read().unwrap();
                    (b.best_bid(), b.best_ask())
                };
                // Alternate aggressive buy/sell pings to create realistic tape flow
                let wobble = ((tick + strat_idx as u64) % 5) * 15;
                if tick.is_multiple_of(2) {
                    let px = best_ask
                        .map(|(a, _)| a + wobble)
                        .unwrap_or(100 * PRICE_SCALE + 25);
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: state.next_id(),
                        side: Side::Buy,
                        shares: lot_size,
                        price: px,
                    });
                } else {
                    let px = best_bid
                        .map(|(b, _)| b.saturating_sub(wobble))
                        .unwrap_or(100 * PRICE_SCALE - 25);
                    let _ = tx.send(OuchInbound::EnterOrder {
                        cl_ord_id: state.next_id(),
                        side: Side::Sell,
                        shares: lot_size,
                        price: px,
                    });
                }

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
        }
    }

    let _ = fmt_price(0);
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
}

