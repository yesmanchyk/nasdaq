//! Minimal binary wire protocol modeled after NASDAQ OUCH 5.0 (Order Entry)
//! and ITCH 5.0 (Market Data Feed).
//!
//! All integers are encoded in Big-Endian (Network Byte Order), matching
//! standard NASDAQ SoupBinTCP / MoldUDP64 conventions.
//! Prices are fixed-point with 4 implied decimal places ($100.2500 -> 1_002_500).

use std::io::{self, Read, Write};

pub const PRICE_SCALE: u64 = 10_000;

pub fn fmt_price(p: u64) -> String {
    format!("{}.{:04}", p / PRICE_SCALE, p % PRICE_SCALE)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Buy = b'B',
    Sell = b'S',
}

impl Side {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            b'B' => Some(Self::Buy),
            b'S' => Some(Self::Sell),
            _ => None,
        }
    }
}

// ============================================================================
// OUCH Inbound (Bot -> Exchange over TCP, 22 bytes fixed frame)
// ============================================================================

#[derive(Debug, Clone, Copy)]
pub enum OuchInbound {
    /// 'O': Enter a new limit order
    EnterOrder {
        cl_ord_id: u64,
        side: Side,
        shares: u32,
        price: u64,
    },
    /// 'X': Cancel an existing order by client order ID
    CancelOrder {
        cl_ord_id: u64,
    },
}

impl OuchInbound {
    pub const WIRE_SIZE: usize = 22;

    pub fn to_bytes(&self) -> [u8; Self::WIRE_SIZE] {
        let mut buf = [0u8; Self::WIRE_SIZE];
        match *self {
            OuchInbound::EnterOrder {
                cl_ord_id,
                side,
                shares,
                price,
            } => {
                buf[0] = b'O';
                buf[1..9].copy_from_slice(&cl_ord_id.to_be_bytes());
                buf[9] = side as u8;
                buf[10..14].copy_from_slice(&shares.to_be_bytes());
                buf[14..22].copy_from_slice(&price.to_be_bytes());
            }
            OuchInbound::CancelOrder { cl_ord_id } => {
                buf[0] = b'X';
                buf[1..9].copy_from_slice(&cl_ord_id.to_be_bytes());
            }
        }
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::WIRE_SIZE]) -> Option<Self> {
        match buf[0] {
            b'O' => {
                let cl_ord_id = u64::from_be_bytes(buf[1..9].try_into().ok()?);
                let side = Side::from_byte(buf[9])?;
                let shares = u32::from_be_bytes(buf[10..14].try_into().ok()?);
                let price = u64::from_be_bytes(buf[14..22].try_into().ok()?);
                Some(OuchInbound::EnterOrder {
                    cl_ord_id,
                    side,
                    shares,
                    price,
                })
            }
            b'X' => {
                let cl_ord_id = u64::from_be_bytes(buf[1..9].try_into().ok()?);
                Some(OuchInbound::CancelOrder { cl_ord_id })
            }
            _ => None,
        }
    }
}

// ============================================================================
// OUCH Outbound (Exchange -> Bot over TCP, 30 bytes fixed frame)
// ============================================================================

#[derive(Debug, Clone, Copy)]
pub enum OuchOutbound {
    /// 'A': Order accepted onto book
    OrderAccepted {
        cl_ord_id: u64,
        order_ref: u64,
        side: Side,
        shares: u32,
        price: u64,
    },
    /// 'E': Order filled (partially or fully)
    OrderExecuted {
        cl_ord_id: u64,
        order_ref: u64,
        side: Side,
        executed_shares: u32,
        fill_price: u64,
    },
    /// 'C': Order canceled
    OrderCanceled {
        cl_ord_id: u64,
        order_ref: u64,
        canceled_shares: u32,
    },
}

impl OuchOutbound {
    pub const WIRE_SIZE: usize = 30;

    pub fn to_bytes(&self) -> [u8; Self::WIRE_SIZE] {
        let mut buf = [0u8; Self::WIRE_SIZE];
        match *self {
            OuchOutbound::OrderAccepted {
                cl_ord_id,
                order_ref,
                side,
                shares,
                price,
            } => {
                buf[0] = b'A';
                buf[1..9].copy_from_slice(&cl_ord_id.to_be_bytes());
                buf[9..17].copy_from_slice(&order_ref.to_be_bytes());
                buf[17] = side as u8;
                buf[18..22].copy_from_slice(&shares.to_be_bytes());
                buf[22..30].copy_from_slice(&price.to_be_bytes());
            }
            OuchOutbound::OrderExecuted {
                cl_ord_id,
                order_ref,
                side,
                executed_shares,
                fill_price,
            } => {
                buf[0] = b'E';
                buf[1..9].copy_from_slice(&cl_ord_id.to_be_bytes());
                buf[9..17].copy_from_slice(&order_ref.to_be_bytes());
                buf[17] = side as u8;
                buf[18..22].copy_from_slice(&executed_shares.to_be_bytes());
                buf[22..30].copy_from_slice(&fill_price.to_be_bytes());
            }
            OuchOutbound::OrderCanceled {
                cl_ord_id,
                order_ref,
                canceled_shares,
            } => {
                buf[0] = b'C';
                buf[1..9].copy_from_slice(&cl_ord_id.to_be_bytes());
                buf[9..17].copy_from_slice(&order_ref.to_be_bytes());
                buf[18..22].copy_from_slice(&canceled_shares.to_be_bytes());
            }
        }
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::WIRE_SIZE]) -> Option<Self> {
        let cl_ord_id = u64::from_be_bytes(buf[1..9].try_into().ok()?);
        let order_ref = u64::from_be_bytes(buf[9..17].try_into().ok()?);
        let shares = u32::from_be_bytes(buf[18..22].try_into().ok()?);
        let price = u64::from_be_bytes(buf[22..30].try_into().ok()?);
        match buf[0] {
            b'A' => Some(OuchOutbound::OrderAccepted {
                cl_ord_id,
                order_ref,
                side: Side::from_byte(buf[17])?,
                shares,
                price,
            }),
            b'E' => Some(OuchOutbound::OrderExecuted {
                cl_ord_id,
                order_ref,
                side: Side::from_byte(buf[17])?,
                executed_shares: shares,
                fill_price: price,
            }),
            b'C' => Some(OuchOutbound::OrderCanceled {
                cl_ord_id,
                order_ref,
                canceled_shares: shares,
            }),
            _ => None,
        }
    }
}

// ============================================================================
// ITCH Broadcast (Exchange -> All Bots over UDP, 30 bytes fixed frame)
// ============================================================================

#[derive(Debug, Clone, Copy)]
pub enum ItchMessage {
    /// 'A': Add Order to book
    OrderAdded {
        seq_num: u64,
        order_ref: u64,
        side: Side,
        shares: u32,
        price: u64,
    },
    /// 'E': Order Executed on book
    OrderExecuted {
        seq_num: u64,
        order_ref: u64,
        side: Side,
        executed_shares: u32,
        match_price: u64,
    },
    /// 'D': Order Deleted from book
    OrderDeleted {
        seq_num: u64,
        order_ref: u64,
    },
}

impl ItchMessage {
    pub const WIRE_SIZE: usize = 30;

    pub fn to_bytes(&self) -> [u8; Self::WIRE_SIZE] {
        let mut buf = [0u8; Self::WIRE_SIZE];
        match *self {
            ItchMessage::OrderAdded {
                seq_num,
                order_ref,
                side,
                shares,
                price,
            } => {
                buf[0] = b'A';
                buf[1..9].copy_from_slice(&seq_num.to_be_bytes());
                buf[9..17].copy_from_slice(&order_ref.to_be_bytes());
                buf[17] = side as u8;
                buf[18..22].copy_from_slice(&shares.to_be_bytes());
                buf[22..30].copy_from_slice(&price.to_be_bytes());
            }
            ItchMessage::OrderExecuted {
                seq_num,
                order_ref,
                side,
                executed_shares,
                match_price,
            } => {
                buf[0] = b'E';
                buf[1..9].copy_from_slice(&seq_num.to_be_bytes());
                buf[9..17].copy_from_slice(&order_ref.to_be_bytes());
                buf[17] = side as u8;
                buf[18..22].copy_from_slice(&executed_shares.to_be_bytes());
                buf[22..30].copy_from_slice(&match_price.to_be_bytes());
            }
            ItchMessage::OrderDeleted { seq_num, order_ref } => {
                buf[0] = b'D';
                buf[1..9].copy_from_slice(&seq_num.to_be_bytes());
                buf[9..17].copy_from_slice(&order_ref.to_be_bytes());
            }
        }
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::WIRE_SIZE]) -> Option<Self> {
        let seq_num = u64::from_be_bytes(buf[1..9].try_into().ok()?);
        let order_ref = u64::from_be_bytes(buf[9..17].try_into().ok()?);
        let shares = u32::from_be_bytes(buf[18..22].try_into().ok()?);
        let price = u64::from_be_bytes(buf[22..30].try_into().ok()?);
        match buf[0] {
            b'A' => Some(ItchMessage::OrderAdded {
                seq_num,
                order_ref,
                side: Side::from_byte(buf[17])?,
                shares,
                price,
            }),
            b'E' => Some(ItchMessage::OrderExecuted {
                seq_num,
                order_ref,
                side: Side::from_byte(buf[17])?,
                executed_shares: shares,
                match_price: price,
            }),
            b'D' => Some(ItchMessage::OrderDeleted { seq_num, order_ref }),
            _ => None,
        }
    }
}

/// Helper to read a fixed-size frame from a non-blocking TCP stream.
/// If the first byte is WouldBlock, returns Ok(None).
/// Once at least 1 byte is read, finishes reading the full fixed frame.
pub fn try_read_frame<const N: usize>(stream: &mut impl Read) -> io::Result<Option<[u8; N]>> {
    let mut buf = [0u8; N];
    match stream.read(&mut buf[..1]) {
        Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof")),
        Ok(1) => {}
        Ok(_) => unreachable!(),
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut offset = 1;
    while offset < N {
        match stream.read(&mut buf[offset..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "eof")),
            Ok(n) => offset += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::hint::spin_loop(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(Some(buf))
}

pub fn write_frame<const N: usize>(stream: &mut impl Write, frame: &[u8; N]) -> io::Result<()> {
    let mut offset = 0;
    while offset < N {
        match stream.write(&frame[offset..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write zero")),
            Ok(n) => offset += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::hint::spin_loop(),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
