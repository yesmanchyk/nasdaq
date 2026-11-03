//! Single-Threaded Price-Time Priority Matching Engine with OUCH (Stream) and ITCH (Datagram).
//!
//! Runs entirely on one thread with non-blocking sockets:
//! - Zero locks or mutex contention inside the matching engine.
//! - Deterministic sequence numbering for ITCH market data broadcasts.

use crate::protocol::{
    ItchMessage, OuchInbound, OuchOutbound, Side, fmt_price, try_read_frame, write_frame,
};
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone)]
struct RestingOrder {
    order_ref: u64,
    client_idx: usize,
    cl_ord_id: u64,
    shares: u32,
}

#[derive(Default)]
pub struct OrderBook {
    /// Bids sorted highest price first, FIFO at each price level
    bids: BTreeMap<Reverse<u64>, VecDeque<RestingOrder>>,
    /// Asks sorted lowest price first, FIFO at each price level
    asks: BTreeMap<u64, VecDeque<RestingOrder>>,
    /// Lookup from (client_idx, cl_ord_id) -> (order_ref, side, price) for fast cancel
    by_cl_ord: HashMap<(usize, u64), (u64, Side, u64)>,
    next_order_ref: u64,
}

pub struct ExchangeStats {
    pub total_orders: u64,
    pub total_cancels: u64,
    pub total_trades: u64,
    pub total_volume: u64,
    pub last_trade_price: Option<u64>,
}

pub struct ExchangeServer {
    ouch_listener: Option<UnixListener>,
    itch_sockets: Vec<UnixDatagram>,
    clients: Vec<Option<UnixStream>>,
    book: OrderBook,
    seq_num: u64,
    pub stats: ExchangeStats,
}

impl ExchangeServer {
    pub fn new_connected(
        ouch_streams: Vec<UnixStream>,
        itch_sockets: Vec<UnixDatagram>,
    ) -> std::io::Result<Self> {
        let mut clients = Vec::with_capacity(ouch_streams.len());
        for s in ouch_streams {
            s.set_nonblocking(true)?;
            clients.push(Some(s));
        }
        for d in &itch_sockets {
            d.set_nonblocking(true)?;
        }
        Ok(Self {
            ouch_listener: None,
            itch_sockets,
            clients,
            book: OrderBook::default(),
            seq_num: 0,
            stats: ExchangeStats {
                total_orders: 0,
                total_cancels: 0,
                total_trades: 0,
                total_volume: 0,
                last_trade_price: None,
            },
        })
    }

    /// Single-threaded non-blocking poll loop
    pub fn run(&mut self, running: Arc<AtomicBool>) {
        while running.load(Ordering::Relaxed) {
            let mut did_work = false;

            // 1. Accept any new OUCH bot connections if listening on a path
            if let Some(listener) = &self.ouch_listener
                && let Ok((stream, _addr)) = listener.accept()
            {
                let _ = stream.set_nonblocking(true);
                self.clients.push(Some(stream));
                did_work = true;
            }

            // 2. Poll each connected OUCH client in round-robin order
            for client_idx in 0..self.clients.len() {
                let maybe_msg = if let Some(stream) = self.clients[client_idx].as_mut() {
                    match try_read_frame::<{ OuchInbound::WIRE_SIZE }>(stream) {
                        Ok(Some(raw)) => OuchInbound::from_bytes(&raw),
                        Ok(None) => None,
                        Err(_) => {
                            self.clients[client_idx] = None;
                            None
                        }
                    }
                } else {
                    None
                };

                if let Some(msg) = maybe_msg {
                    did_work = true;
                    self.handle_ouch(client_idx, msg);
                }
            }

            if !did_work {
                std::hint::spin_loop();
            }
        }
    }

    fn handle_ouch(&mut self, client_idx: usize, msg: OuchInbound) {
        match msg {
            OuchInbound::EnterOrder {
                cl_ord_id,
                side,
                shares,
                price,
            } => {
                self.stats.total_orders += 1;
                self.match_or_rest(client_idx, cl_ord_id, side, shares, price);
            }
            OuchInbound::CancelOrder { cl_ord_id } => {
                self.cancel_order(client_idx, cl_ord_id);
            }
        }
    }

    fn match_or_rest(
        &mut self,
        client_idx: usize,
        cl_ord_id: u64,
        side: Side,
        mut remaining: u32,
        price: u64,
    ) {
        self.book.next_order_ref += 1;
        let incoming_ref = self.book.next_order_ref;

        // Cross against opposite side of book
        match side {
            Side::Buy => {
                while remaining > 0 {
                    let best_ask_price = match self.book.asks.keys().next().copied() {
                        Some(p) if p <= price => p,
                        _ => break,
                    };

                    let mut remove_level = false;
                    let mut executions = Vec::new();
                    if let Some(queue) = self.book.asks.get_mut(&best_ask_price) {
                        while remaining > 0 {
                            let Some(resting) = queue.front_mut() else {
                                break;
                            };
                            let fill_qty = remaining.min(resting.shares);
                            remaining -= fill_qty;
                            resting.shares -= fill_qty;

                            let resting_ref = resting.order_ref;
                            let resting_client = resting.client_idx;
                            let resting_cl_ord = resting.cl_ord_id;
                            let fully_filled = resting.shares == 0;

                            if fully_filled {
                                queue.pop_front();
                                self.book
                                    .by_cl_ord
                                    .remove(&(resting_client, resting_cl_ord));
                            }

                            executions.push((
                                resting_client,
                                resting_cl_ord,
                                resting_ref,
                                fill_qty,
                            ));
                        }
                        if queue.is_empty() {
                            remove_level = true;
                        }
                    }
                    if remove_level {
                        self.book.asks.remove(&best_ask_price);
                    }
                    for (resting_client, resting_cl_ord, resting_ref, fill_qty) in executions {
                        self.record_execution(
                            client_idx,
                            cl_ord_id,
                            incoming_ref,
                            Side::Buy,
                            resting_client,
                            resting_cl_ord,
                            resting_ref,
                            Side::Sell,
                            fill_qty,
                            best_ask_price,
                        );
                    }
                }
            }
            Side::Sell => {
                while remaining > 0 {
                    let best_bid_price = match self.book.bids.keys().next().copied() {
                        Some(Reverse(p)) if p >= price => p,
                        _ => break,
                    };

                    let mut remove_level = false;
                    let mut executions = Vec::new();
                    if let Some(queue) = self.book.bids.get_mut(&Reverse(best_bid_price)) {
                        while remaining > 0 {
                            let Some(resting) = queue.front_mut() else {
                                break;
                            };
                            let fill_qty = remaining.min(resting.shares);
                            remaining -= fill_qty;
                            resting.shares -= fill_qty;

                            let resting_ref = resting.order_ref;
                            let resting_client = resting.client_idx;
                            let resting_cl_ord = resting.cl_ord_id;
                            let fully_filled = resting.shares == 0;

                            if fully_filled {
                                queue.pop_front();
                                self.book
                                    .by_cl_ord
                                    .remove(&(resting_client, resting_cl_ord));
                            }

                            executions.push((
                                resting_client,
                                resting_cl_ord,
                                resting_ref,
                                fill_qty,
                            ));
                        }
                        if queue.is_empty() {
                            remove_level = true;
                        }
                    }
                    if remove_level {
                        self.book.bids.remove(&Reverse(best_bid_price));
                    }
                    for (resting_client, resting_cl_ord, resting_ref, fill_qty) in executions {
                        self.record_execution(
                            client_idx,
                            cl_ord_id,
                            incoming_ref,
                            Side::Sell,
                            resting_client,
                            resting_cl_ord,
                            resting_ref,
                            Side::Buy,
                            fill_qty,
                            best_bid_price,
                        );
                    }
                }
            }
        }

        // Rest any residual quantity on the book
        if remaining > 0 {
            let resting = RestingOrder {
                order_ref: incoming_ref,
                client_idx,
                cl_ord_id,
                shares: remaining,
            };
            match side {
                Side::Buy => self
                    .book
                    .bids
                    .entry(Reverse(price))
                    .or_default()
                    .push_back(resting),
                Side::Sell => self
                    .book
                    .asks
                    .entry(price)
                    .or_default()
                    .push_back(resting),
            }
            self.book
                .by_cl_ord
                .insert((client_idx, cl_ord_id), (incoming_ref, side, price));

            // OUCH OrderAccepted back to submitting bot
            self.send_ouch(
                client_idx,
                OuchOutbound::OrderAccepted {
                    cl_ord_id,
                    order_ref: incoming_ref,
                    side,
                    shares: remaining,
                    price,
                },
            );

            // ITCH OrderAdded broadcast to all market participants
            self.seq_num += 1;
            self.broadcast_itch(ItchMessage::OrderAdded {
                seq_num: self.seq_num,
                order_ref: incoming_ref,
                side,
                shares: remaining,
                price,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn record_execution(
        &mut self,
        aggressor_client: usize,
        aggressor_cl_ord: u64,
        aggressor_ref: u64,
        aggressor_side: Side,
        resting_client: usize,
        resting_cl_ord: u64,
        resting_ref: u64,
        resting_side: Side,
        fill_qty: u32,
        fill_price: u64,
    ) {
        self.stats.total_trades += 1;
        self.stats.total_volume += fill_qty as u64;
        self.stats.last_trade_price = Some(fill_price);

        // Private OUCH execution report to resting maker
        self.send_ouch(
            resting_client,
            OuchOutbound::OrderExecuted {
                cl_ord_id: resting_cl_ord,
                order_ref: resting_ref,
                side: resting_side,
                executed_shares: fill_qty,
                fill_price,
            },
        );

        // Private OUCH execution report to aggressive taker
        self.send_ouch(
            aggressor_client,
            OuchOutbound::OrderExecuted {
                cl_ord_id: aggressor_cl_ord,
                order_ref: aggressor_ref,
                side: aggressor_side,
                executed_shares: fill_qty,
                fill_price,
            },
        );

        // Public ITCH OrderExecuted broadcast
        self.seq_num += 1;
        self.broadcast_itch(ItchMessage::OrderExecuted {
            seq_num: self.seq_num,
            order_ref: resting_ref,
            side: resting_side,
            executed_shares: fill_qty,
            match_price: fill_price,
        });
    }

    fn cancel_order(&mut self, client_idx: usize, cl_ord_id: u64) {
        let Some((order_ref, side, price)) = self.book.by_cl_ord.remove(&(client_idx, cl_ord_id))
        else {
            return;
        };

        let mut canceled_shares = 0;
        match side {
            Side::Buy => {
                let mut empty = false;
                if let Some(queue) = self.book.bids.get_mut(&Reverse(price)) {
                    if let Some(pos) = queue.iter().position(|o| o.order_ref == order_ref) {
                        canceled_shares = queue.remove(pos).unwrap().shares;
                    }
                    empty = queue.is_empty();
                }
                if empty {
                    self.book.bids.remove(&Reverse(price));
                }
            }
            Side::Sell => {
                let mut empty = false;
                if let Some(queue) = self.book.asks.get_mut(&price) {
                    if let Some(pos) = queue.iter().position(|o| o.order_ref == order_ref) {
                        canceled_shares = queue.remove(pos).unwrap().shares;
                    }
                    empty = queue.is_empty();
                }
                if empty {
                    self.book.asks.remove(&price);
                }
            }
        }

        if canceled_shares > 0 {
            self.stats.total_cancels += 1;
            self.send_ouch(
                client_idx,
                OuchOutbound::OrderCanceled {
                    cl_ord_id,
                    order_ref,
                    canceled_shares,
                },
            );
            self.seq_num += 1;
            self.broadcast_itch(ItchMessage::OrderDeleted {
                seq_num: self.seq_num,
                order_ref,
            });
        }
    }

    fn send_ouch(&mut self, client_idx: usize, msg: OuchOutbound) {
        if let Some(Some(stream)) = self.clients.get_mut(client_idx) {
            let bytes = msg.to_bytes();
            if write_frame(stream, &bytes).is_err() {
                self.clients[client_idx] = None;
            }
        }
    }

    fn broadcast_itch(&self, msg: ItchMessage) {
        let bytes = msg.to_bytes();
        for sock in &self.itch_sockets {
            let _ = sock.send(&bytes);
        }
    }

    pub fn summary(&self) -> String {
        let best_bid = self
            .book
            .bids
            .iter()
            .next()
            .map(|(Reverse(p), q)| {
                let qty: u32 = q.iter().map(|o| o.shares).sum();
                format!("{} ({} sh)", fmt_price(*p), qty)
            })
            .unwrap_or_else(|| "None".to_string());

        let best_ask = self
            .book
            .asks
            .iter()
            .next()
            .map(|(p, q)| {
                let qty: u32 = q.iter().map(|o| o.shares).sum();
                format!("{} ({} sh)", fmt_price(*p), qty)
            })
            .unwrap_or_else(|| "None".to_string());

        let last_px = self
            .stats
            .last_trade_price
            .map(fmt_price)
            .unwrap_or_else(|| "N/A".to_string());

        format!(
            "Orders: {} | Cancels: {} | Trades: {} | Vol: {} sh | LastPx: ${} | BBO: [Bid ${} x Ask ${}]",
            self.stats.total_orders,
            self.stats.total_cancels,
            self.stats.total_trades,
            self.stats.total_volume,
            last_px,
            best_bid,
            best_ask
        )
    }
}
