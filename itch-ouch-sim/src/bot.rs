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
                match side {
                    Side::Sell => self.recent_buy_exec_vol += executed_shares as u64,
                    Side::Buy => self.recent_sell_exec_vol += executed_shares as u64,
                }
                self.reduce_order(order_ref, executed_shares);
            }
            ItchMessage::OrderDeleted { order_ref, .. } => {
                self.remove_order(order_ref);
            }
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
}

pub fn default_strategies_for_bot(bot_id: u64) -> (&'static str, [StrategyKind; 2]) {
    if bot_id == 1 {
        (
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
        )
    } else {
        (
            "Bot-2 (Mom + Taker) ",
            [
                StrategyKind::MomentumSniper { lot_size: 40 },
                StrategyKind::LiquidityTaker { lot_size: 60 },
            ],
        )
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
        }
    }

    let _ = fmt_price(0);
}
