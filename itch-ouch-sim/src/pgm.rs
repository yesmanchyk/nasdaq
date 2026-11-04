//! Pragmatic General Multicast (PGM - RFC 3208) Reliable Multicast Layer for ITCH 5.0.
//!
//! Uses UDP-Encapsulated PGM over IPv4 Multicast (`239.255.0.1:30001`) with a
//! unicast NAK backchannel (`127.0.0.1:30002`) so it runs without root (`CAP_NET_RAW`)
//! while providing full RFC 3208 PGM reliability semantics:
//! - `PGM_SPM`   (0x00): Source Path Message (heartbeat with lead/trail window state)
//! - `PGM_ODATA` (0x04): Original Data carrying ITCH payload + transmit window trail
//! - `PGM_RDATA` (0x05): Repair Data retransmitted from the Exchange's TX ring buffer
//! - `PGM_NAK`   (0x08): Negative Acknowledgment requesting missing sequence range
//! - `PGM_NCF`   (0x09): NAK Confirmation acknowledging repair in progress

use crate::protocol::ItchMessage;
use std::collections::BTreeMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};

pub const DEFAULT_MCAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 0, 1);
pub const DEFAULT_MCAST_PORT: u16 = 30001;
pub const DEFAULT_NAK_ADDR: &str = "127.0.0.1:30002";
pub const DEFAULT_OUCH_ADDR: &str = "127.0.0.1:40000";

const PGM_TX_WINDOW_CAPACITY: usize = 65_536;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PgmPacketType {
    Spm = 0x00,
    Odata = 0x04,
    Rdata = 0x05,
    Nak = 0x08,
    Ncf = 0x09,
}

/// Fixed 52-byte PGM wire frame (22-byte RFC 3208 PGM header + 30-byte ITCH payload)
#[derive(Debug, Clone, Copy)]
pub enum PgmFrame {
    /// Source Path Message: announces current sequence lead and transmit window trail
    Spm {
        tsi: u32,
        sqn_lead: u64,
        txw_trail: u64,
    },
    /// Original Data: primary multicast transmission of an ITCH message
    Odata {
        tsi: u32,
        sqn: u64,
        txw_trail: u64,
        itch: ItchMessage,
    },
    /// Repair Data: retransmitted ITCH message in response to a NAK
    Rdata {
        tsi: u32,
        sqn: u64,
        txw_trail: u64,
        itch: ItchMessage,
    },
    /// Negative Acknowledgment: requests repair for [missing_start..=missing_end]
    Nak {
        tsi: u32,
        missing_start: u64,
        missing_end: u64,
    },
    /// NAK Confirmation: sent by source to suppress duplicate NAKs while RDATA is sent
    Ncf {
        tsi: u32,
        repair_start: u64,
        repair_end: u64,
    },
}

impl PgmFrame {
    pub const WIRE_SIZE: usize = 22 + ItchMessage::WIRE_SIZE; // 52 bytes

    pub fn to_bytes(&self) -> [u8; Self::WIRE_SIZE] {
        let mut buf = [0u8; Self::WIRE_SIZE];
        // Bytes 0..2: PGM version/flags (0x81 = PGM v1)
        buf[0] = 0x81;
        match *self {
            PgmFrame::Spm {
                tsi,
                sqn_lead,
                txw_trail,
            } => {
                buf[1] = PgmPacketType::Spm as u8;
                buf[2..6].copy_from_slice(&tsi.to_be_bytes());
                buf[6..14].copy_from_slice(&sqn_lead.to_be_bytes());
                buf[14..22].copy_from_slice(&txw_trail.to_be_bytes());
            }
            PgmFrame::Odata {
                tsi,
                sqn,
                txw_trail,
                itch,
            } => {
                buf[1] = PgmPacketType::Odata as u8;
                buf[2..6].copy_from_slice(&tsi.to_be_bytes());
                buf[6..14].copy_from_slice(&sqn.to_be_bytes());
                buf[14..22].copy_from_slice(&txw_trail.to_be_bytes());
                buf[22..52].copy_from_slice(&itch.to_bytes());
            }
            PgmFrame::Rdata {
                tsi,
                sqn,
                txw_trail,
                itch,
            } => {
                buf[1] = PgmPacketType::Rdata as u8;
                buf[2..6].copy_from_slice(&tsi.to_be_bytes());
                buf[6..14].copy_from_slice(&sqn.to_be_bytes());
                buf[14..22].copy_from_slice(&txw_trail.to_be_bytes());
                buf[22..52].copy_from_slice(&itch.to_bytes());
            }
            PgmFrame::Nak {
                tsi,
                missing_start,
                missing_end,
            } => {
                buf[1] = PgmPacketType::Nak as u8;
                buf[2..6].copy_from_slice(&tsi.to_be_bytes());
                buf[6..14].copy_from_slice(&missing_start.to_be_bytes());
                buf[14..22].copy_from_slice(&missing_end.to_be_bytes());
            }
            PgmFrame::Ncf {
                tsi,
                repair_start,
                repair_end,
            } => {
                buf[1] = PgmPacketType::Ncf as u8;
                buf[2..6].copy_from_slice(&tsi.to_be_bytes());
                buf[6..14].copy_from_slice(&repair_start.to_be_bytes());
                buf[14..22].copy_from_slice(&repair_end.to_be_bytes());
            }
        }
        buf
    }

    pub fn from_bytes(buf: &[u8; Self::WIRE_SIZE]) -> Option<Self> {
        if buf[0] != 0x81 {
            return None;
        }
        let tsi = u32::from_be_bytes(buf[2..6].try_into().ok()?);
        let field_a = u64::from_be_bytes(buf[6..14].try_into().ok()?);
        let field_b = u64::from_be_bytes(buf[14..22].try_into().ok()?);
        let itch_slice: &[u8; ItchMessage::WIRE_SIZE] = buf[22..52].try_into().ok()?;

        match buf[1] {
            0x00 => Some(PgmFrame::Spm {
                tsi,
                sqn_lead: field_a,
                txw_trail: field_b,
            }),
            0x04 => Some(PgmFrame::Odata {
                tsi,
                sqn: field_a,
                txw_trail: field_b,
                itch: ItchMessage::from_bytes(itch_slice)?,
            }),
            0x05 => Some(PgmFrame::Rdata {
                tsi,
                sqn: field_a,
                txw_trail: field_b,
                itch: ItchMessage::from_bytes(itch_slice)?,
            }),
            0x08 => Some(PgmFrame::Nak {
                tsi,
                missing_start: field_a,
                missing_end: field_b,
            }),
            0x09 => Some(PgmFrame::Ncf {
                tsi,
                repair_start: field_a,
                repair_end: field_b,
            }),
            _ => None,
        }
    }
}

// ============================================================================
// PGM Sender (Exchange Side: Multicast ODATA/SPM + Ring Buffer + NAK Responder)
// ============================================================================

pub struct PgmPublisher {
    mcast_sock: UdpSocket,
    mcast_dest: SocketAddrV4,
    nak_sock: UdpSocket,
    tsi: u32,
    sqn_lead: u64,
    tx_window: Vec<Option<(u64, ItchMessage)>>,
    pub odata_sent: u64,
    pub rdata_sent: u64,
    pub spm_sent: u64,
    pub naks_received: u64,
}

impl PgmPublisher {
    pub fn bind(
        mcast_group: Ipv4Addr,
        mcast_port: u16,
        nak_bind_addr: &str,
        tsi: u32,
    ) -> io::Result<Self> {
        let mcast_sock = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
        mcast_sock.set_nonblocking(true)?;
        mcast_sock.set_multicast_loop_v4(true)?;
        mcast_sock.set_multicast_ttl_v4(1)?;
        set_multicast_if_loopback(&mcast_sock)?;

        let nak_sock = UdpSocket::bind(nak_bind_addr)?;
        nak_sock.set_nonblocking(true)?;

        Ok(Self {
            mcast_sock,
            mcast_dest: SocketAddrV4::new(mcast_group, mcast_port),
            nak_sock,
            tsi,
            sqn_lead: 0,
            tx_window: vec![None; PGM_TX_WINDOW_CAPACITY],
            odata_sent: 0,
            rdata_sent: 0,
            spm_sent: 0,
            naks_received: 0,
        })
    }

    pub fn txw_trail(&self) -> u64 {
        self.sqn_lead
            .saturating_sub(PGM_TX_WINDOW_CAPACITY as u64 - 1)
            .max(1)
    }

    /// Publish an ITCH message inside a PGM ODATA frame and record it in the TX window
    pub fn publish_itch(&mut self, itch: ItchMessage) {
        self.sqn_lead += 1;
        let sqn = self.sqn_lead;
        let slot = (sqn as usize) % PGM_TX_WINDOW_CAPACITY;
        self.tx_window[slot] = Some((sqn, itch));

        let frame = PgmFrame::Odata {
            tsi: self.tsi,
            sqn,
            txw_trail: self.txw_trail(),
            itch,
        };
        let _ = self.mcast_sock.send_to(&frame.to_bytes(), self.mcast_dest);
        self.odata_sent += 1;
    }

    /// Broadcast a periodic PGM Source Path Message (SPM) so receivers detect trailing gaps
    pub fn send_spm(&mut self) {
        let frame = PgmFrame::Spm {
            tsi: self.tsi,
            sqn_lead: self.sqn_lead,
            txw_trail: self.txw_trail(),
        };
        let _ = self.mcast_sock.send_to(&frame.to_bytes(), self.mcast_dest);
        self.spm_sent += 1;
    }

    /// Non-blocking poll for incoming PGM NAK requests; emits NCF + RDATA repairs
    pub fn poll_naks(&mut self) -> bool {
        let mut buf = [0u8; PgmFrame::WIRE_SIZE];
        let Ok((n, _peer)) = self.nak_sock.recv_from(&mut buf) else {
            return false;
        };
        if n != PgmFrame::WIRE_SIZE {
            return false;
        }
        let Some(PgmFrame::Nak {
            tsi,
            missing_start,
            missing_end,
        }) = PgmFrame::from_bytes(&buf)
        else {
            return false;
        };
        if tsi != self.tsi {
            return false;
        }

        self.naks_received += 1;
        let start = missing_start.max(self.txw_trail());
        let end = missing_end.min(self.sqn_lead).min(start + 64);
        if start > end {
            return true;
        }

        // 1. Multicast NCF (NAK Confirmation) per RFC 3208
        let ncf = PgmFrame::Ncf {
            tsi: self.tsi,
            repair_start: start,
            repair_end: end,
        };
        let _ = self.mcast_sock.send_to(&ncf.to_bytes(), self.mcast_dest);

        // 2. Multicast RDATA (Repair Data) for each requested sequence number in window
        let trail = self.txw_trail();
        for sqn in start..=end {
            let slot = (sqn as usize) % PGM_TX_WINDOW_CAPACITY;
            if let Some((stored_sqn, itch)) = self.tx_window[slot]
                && stored_sqn == sqn
            {
                let rdata = PgmFrame::Rdata {
                    tsi: self.tsi,
                    sqn,
                    txw_trail: trail,
                    itch,
                };
                let _ = self.mcast_sock.send_to(&rdata.to_bytes(), self.mcast_dest);
                self.rdata_sent += 1;
            }
        }
        true
    }
}

// ============================================================================
// PGM Receiver (Bot Side: Multicast Listener + Reorder Buffer + NAK Generator)
// ============================================================================

pub struct PgmSubscriber {
    mcast_sock: UdpSocket,
    nak_sock: UdpSocket,
    nak_target: SocketAddr,
    expected_sqn: u64,
    reorder_buf: BTreeMap<u64, ItchMessage>,
    pub odata_recv: u64,
    pub rdata_recv: u64,
    pub naks_sent: u64,
}

impl PgmSubscriber {
    pub fn join(
        mcast_group: Ipv4Addr,
        mcast_port: u16,
        nak_target_addr: &str,
    ) -> io::Result<Self> {
        let mcast_sock = bind_shared_multicast_udp(mcast_group, mcast_port)?;
        mcast_sock.set_nonblocking(true)?;

        let nak_sock = UdpSocket::bind("127.0.0.1:0")?;
        nak_sock.set_nonblocking(true)?;
        let nak_target: SocketAddr = nak_target_addr
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        Ok(Self {
            mcast_sock,
            nak_sock,
            nak_target,
            expected_sqn: 1,
            reorder_buf: BTreeMap::new(),
            odata_recv: 0,
            rdata_recv: 0,
            naks_sent: 0,
        })
    }

    /// Reads next available PGM frame and returns in-order `ItchMessage`s.
    /// Automatically transmits PGM `NAK` frames when sequence gaps are detected.
    pub fn poll_itch(&mut self, out: &mut Vec<ItchMessage>) -> io::Result<bool> {
        let mut buf = [0u8; PgmFrame::WIRE_SIZE];
        match self.mcast_sock.recv_from(&mut buf) {
            Ok((n, _)) if n == PgmFrame::WIRE_SIZE => {
                if let Some(frame) = PgmFrame::from_bytes(&buf) {
                    self.handle_frame(frame, out);
                    return Ok(true);
                }
                Ok(false)
            }
            Ok(_) => Ok(false),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn handle_frame(&mut self, frame: PgmFrame, out: &mut Vec<ItchMessage>) {
        match frame {
            PgmFrame::Odata {
                tsi,
                sqn,
                txw_trail,
                itch,
            }
            | PgmFrame::Rdata {
                tsi,
                sqn,
                txw_trail,
                itch,
            } => {
                if matches!(frame, PgmFrame::Odata { .. }) {
                    self.odata_recv += 1;
                } else {
                    self.rdata_recv += 1;
                }

                // Fast-forward if receiver joined late and trail moved past expected_sqn
                if self.expected_sqn == 1 && sqn > 1 && txw_trail > self.expected_sqn {
                    self.expected_sqn = txw_trail;
                }

                if sqn == self.expected_sqn {
                    out.push(itch);
                    self.expected_sqn += 1;
                    while let Some(next_itch) = self.reorder_buf.remove(&self.expected_sqn) {
                        out.push(next_itch);
                        self.expected_sqn += 1;
                    }
                } else if sqn > self.expected_sqn {
                    // Gap detected! Buffer out-of-order packet and send PGM NAK
                    self.reorder_buf.insert(sqn, itch);
                    let nak = PgmFrame::Nak {
                        tsi,
                        missing_start: self.expected_sqn,
                        missing_end: sqn - 1,
                    };
                    let _ = self.nak_sock.send_to(&nak.to_bytes(), self.nak_target);
                    self.naks_sent += 1;
                }
            }
            PgmFrame::Spm {
                tsi,
                sqn_lead,
                txw_trail,
            } => {
                if self.expected_sqn < txw_trail {
                    self.expected_sqn = txw_trail;
                }
                if sqn_lead >= self.expected_sqn {
                    let nak = PgmFrame::Nak {
                        tsi,
                        missing_start: self.expected_sqn,
                        missing_end: sqn_lead,
                    };
                    let _ = self.nak_sock.send_to(&nak.to_bytes(), self.nak_target);
                    self.naks_sent += 1;
                }
            }
            PgmFrame::Nak { .. } | PgmFrame::Ncf { .. } => {}
        }
    }
}

// ============================================================================
// Multicast Socket Setup (SO_REUSEADDR + SO_REUSEPORT for Multi-Bot Host)
// ============================================================================

#[cfg(unix)]
unsafe extern "C" {
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn setsockopt(socket: i32, level: i32, name: i32, value: *const u8, option_len: u32) -> i32;
    fn bind(socket: i32, address: *const u8, address_len: u32) -> i32;
}

#[cfg(windows)]
#[link(name = "ws2_32")]
unsafe extern "system" {
    fn WSAStartup(wVersionRequested: u16, lpWSAData: *mut u8) -> i32;
    fn socket(af: i32, type_: i32, protocol: i32) -> RawSocket;
    fn setsockopt(s: RawSocket, level: i32, optname: i32, optval: *const u8, optlen: i32) -> i32;
    fn bind(s: RawSocket, name: *const u8, namelen: i32) -> i32;
    fn closesocket(s: RawSocket) -> i32;
}

#[cfg(unix)]
fn bind_shared_multicast_udp(mcast_group: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    use std::os::fd::FromRawFd;

    #[cfg(target_os = "macos")]
    const AF_INET: i32 = 2;
    #[cfg(not(target_os = "macos"))]
    const AF_INET: i32 = 2;

    const SOCK_DGRAM: i32 = 2;

    #[cfg(target_os = "macos")]
    const SOL_SOCKET: i32 = 0xffff;
    #[cfg(not(target_os = "macos"))]
    const SOL_SOCKET: i32 = 1;

    #[cfg(target_os = "macos")]
    const SO_REUSEADDR: i32 = 0x0004;
    #[cfg(not(target_os = "macos"))]
    const SO_REUSEADDR: i32 = 2;

    #[cfg(target_os = "macos")]
    const SO_REUSEPORT: i32 = 0x0200;
    #[cfg(not(target_os = "macos"))]
    const SO_REUSEPORT: i32 = 15;

    let fd = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let one: i32 = 1;
    unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_REUSEADDR,
            (&raw const one).cast(),
            size_of::<i32>() as u32,
        );
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_REUSEPORT,
            (&raw const one).cast(),
            size_of::<i32>() as u32,
        );
    }

    #[repr(C)]
    struct SockAddrIn {
        #[cfg(target_os = "macos")]
        sin_len: u8,
        #[cfg(target_os = "macos")]
        sin_family: u8,
        #[cfg(not(target_os = "macos"))]
        sin_family: u16,
        sin_port: u16,
        sin_addr: u32,
        sin_zero: [u8; 8],
    }

    let addr = SockAddrIn {
        #[cfg(target_os = "macos")]
        sin_len: size_of::<SockAddrIn>() as u8,
        #[cfg(target_os = "macos")]
        sin_family: AF_INET as u8,
        #[cfg(not(target_os = "macos"))]
        sin_family: AF_INET as u16,
        sin_port: port.to_be(),
        sin_addr: 0, // INADDR_ANY (0.0.0.0)
        sin_zero: [0; 8],
    };

    let rc = unsafe {
        bind(
            fd,
            (&raw const addr).cast(),
            size_of::<SockAddrIn>() as u32,
        )
    };
    let sock = unsafe { UdpSocket::from_raw_fd(fd) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    sock.join_multicast_v4(&mcast_group, &Ipv4Addr::LOCALHOST)?;
    let _ = sock.as_raw_fd();
    Ok(sock)
}

#[cfg(windows)]
fn bind_shared_multicast_udp(mcast_group: Ipv4Addr, port: u16) -> io::Result<UdpSocket> {
    const AF_INET: i32 = 2;
    const SOCK_DGRAM: i32 = 2;
    const IPPROTO_UDP: i32 = 17;
    const SOL_SOCKET: i32 = 0xffff;
    const SO_REUSEADDR: i32 = 0x0004;
    const INVALID_SOCKET: RawSocket = !0;

    let mut wsa_data = [0u8; 512];
    let wsa_rc = unsafe { WSAStartup(0x0202, wsa_data.as_mut_ptr()) };
    if wsa_rc != 0 {
        return Err(io::Error::from_raw_os_error(wsa_rc));
    }

    let raw_sock = unsafe { socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP) };
    if raw_sock == INVALID_SOCKET {
        return Err(io::Error::last_os_error());
    }

    let one: i32 = 1;
    let opt_rc = unsafe {
        setsockopt(
            raw_sock,
            SOL_SOCKET,
            SO_REUSEADDR,
            (&raw const one).cast(),
            size_of::<i32>() as i32,
        )
    };
    if opt_rc != 0 {
        let err = io::Error::last_os_error();
        unsafe { closesocket(raw_sock) };
        return Err(err);
    }

    #[repr(C)]
    struct SockAddrIn {
        sin_family: u16,
        sin_port: u16,
        sin_addr: u32,
        sin_zero: [u8; 8],
    }

    let addr = SockAddrIn {
        sin_family: AF_INET as u16,
        sin_port: port.to_be(),
        sin_addr: 0, // INADDR_ANY (0.0.0.0)
        sin_zero: [0; 8],
    };

    let rc = unsafe {
        bind(
            raw_sock,
            (&raw const addr).cast(),
            size_of::<SockAddrIn>() as i32,
        )
    };
    let sock = unsafe { UdpSocket::from_raw_socket(raw_sock) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    sock.join_multicast_v4(&mcast_group, &Ipv4Addr::LOCALHOST)?;
    let _ = sock.as_raw_socket();
    Ok(sock)
}

#[cfg(unix)]
fn set_multicast_if_loopback(sock: &UdpSocket) -> io::Result<()> {
    const IPPROTO_IP: i32 = 0;
    #[cfg(target_os = "macos")]
    const IP_MULTICAST_IF: i32 = 9;
    #[cfg(not(target_os = "macos"))]
    const IP_MULTICAST_IF: i32 = 32;

    let loopback_be: u32 = u32::from_ne_bytes(Ipv4Addr::LOCALHOST.octets());
    let rc = unsafe {
        setsockopt(
            sock.as_raw_fd(),
            IPPROTO_IP,
            IP_MULTICAST_IF,
            (&raw const loopback_be).cast(),
            size_of::<u32>() as u32,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn set_multicast_if_loopback(sock: &UdpSocket) -> io::Result<()> {
    const IPPROTO_IP: i32 = 0;
    const IP_MULTICAST_IF: i32 = 9;

    let loopback_be: u32 = u32::from_ne_bytes(Ipv4Addr::LOCALHOST.octets());
    let rc = unsafe {
        setsockopt(
            sock.as_raw_socket(),
            IPPROTO_IP,
            IP_MULTICAST_IF,
            (&raw const loopback_be).cast(),
            size_of::<u32>() as i32,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
