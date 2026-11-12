//! Built-in WireGuard. Runs entirely in userspace (boringtun for WireGuard, smoltcp
//! for TCP/IP), so neither the server nor the agents need root, a TUN device or a VPN client.
//!
//! The server listens on one UDP port. Each agent has its own key pair and a fixed tunnel IP.
//! TCP connections that reach the server's tunnel IP on port 80 are forwarded to the local
//! HTTP server, so everything (agent channel, registry, API) works through the tunnel unchanged.
//! On the agent side, local TCP ports are forwarded into the tunnel, e.g. `localhost:5000`
//! for the registry, which lets Docker pull over plain HTTP without certificates.

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use boringtun::noise::handshake::parse_handshake_anon;
use boringtun::noise::{Packet, Tunn, TunnResult};
use boringtun::x25519::{PublicKey, StaticSecret};
use serde::{Deserialize, Serialize};
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint};
use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Notify, mpsc};

pub const SERVER_IP: Ipv4Addr = Ipv4Addr::new(10, 77, 0, 1);
pub const TUNNEL_PORT: u16 = 80;
const MTU: usize = 1420;
const TCP_BUF: usize = 256 * 1024;
const CHANNEL_DEPTH: usize = 64;
const LISTEN_BACKLOG: usize = 8;

// ── Keys ─────────────────────────────────────────────────────────────────────

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn key_from_b64(s: &str) -> Result<[u8; 32]> {
    let v = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .context("invalid key")?;
    v.try_into().map_err(|_| anyhow!("key must be 32 bytes"))
}

pub fn new_private_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    std::io::Read::read_exact(
        &mut std::fs::File::open("/dev/urandom").expect("urandom"),
        &mut k,
    )
    .expect("urandom");
    k
}

pub fn public_key(private: &[u8; 32]) -> [u8; 32] {
    PublicKey::from(&StaticSecret::from(*private)).to_bytes()
}

/// Tunnel IP of an agent: 10.77.x.y from its id (10.77.0.1 is the server).
pub fn agent_ip(agent_id: i64) -> Ipv4Addr {
    let n = (agent_id + 1) as u32;
    Ipv4Addr::new(10, 77, (n >> 8) as u8, (n & 0xff) as u8)
}

/// Everything an agent needs to join, encoded as one copy-pasteable string.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JoinCode {
    /// Server WireGuard endpoint, host:port.
    pub endpoint: String,
    pub server_public: String,
    pub private: String,
    pub ip: Ipv4Addr,
    /// Agent token, used for the agent channel and registry auth inside the tunnel.
    pub token: String,
}

impl JoinCode {
    pub fn encode(&self) -> String {
        format!(
            "mlkjoin_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(self).unwrap())
        )
    }

    pub fn decode(s: &str) -> Result<Self> {
        let raw = s
            .trim()
            .strip_prefix("mlkjoin_")
            .ok_or_else(|| anyhow!("not a moloko join code"))?;
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw)
            .context("invalid join code")?;
        Ok(serde_json::from_slice(&json)?)
    }
}

// ── Netstack ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// Small unique number; routes data packets back to this peer.
    pub index: u32,
    pub public: [u8; 32],
    pub ip: Ipv4Addr,
    pub endpoint: Option<SocketAddr>,
    pub keepalive: Option<u16>,
}

struct Peer {
    cfg: PeerConfig,
    tunn: Tunn,
    endpoint: Option<SocketAddr>,
}

enum Cmd {
    AddPeer(PeerConfig),
    RemovePeer([u8; 32]),
    Connect {
        remote: (Ipv4Addr, u16),
        bridge: NetSide,
    },
}

/// The netstack's half of a bridged TCP connection.
struct NetSide {
    to_net: mpsc::Receiver<Vec<u8>>,
    from_net: mpsc::Sender<Vec<u8>>,
}

/// The tokio half of a bridged TCP connection.
struct LocalSide {
    to_net: mpsc::Sender<Vec<u8>>,
    from_net: mpsc::Receiver<Vec<u8>>,
}

fn bridge_pair() -> (NetSide, LocalSide) {
    let (a_tx, a_rx) = mpsc::channel(CHANNEL_DEPTH);
    let (b_tx, b_rx) = mpsc::channel(CHANNEL_DEPTH);
    (
        NetSide {
            to_net: a_rx,
            from_net: b_tx,
        },
        LocalSide {
            to_net: a_tx,
            from_net: b_rx,
        },
    )
}

#[derive(Clone)]
pub struct Netstack {
    cmds: mpsc::UnboundedSender<Cmd>,
    wake: Arc<Notify>,
}

impl Netstack {
    pub fn add_peer(&self, p: PeerConfig) {
        let _ = self.cmds.send(Cmd::AddPeer(p));
        self.wake.notify_one();
    }

    pub fn remove_peer(&self, public: [u8; 32]) {
        let _ = self.cmds.send(Cmd::RemovePeer(public));
        self.wake.notify_one();
    }

    /// Forward every connection accepted on `listener` to `remote` inside the tunnel.
    pub fn forward_local(&self, listener: TcpListener, remote: (Ipv4Addr, u16)) {
        let this = self.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let _ = stream.set_nodelay(true);
                let (net, local) = bridge_pair();
                if this
                    .cmds
                    .send(Cmd::Connect {
                        remote,
                        bridge: net,
                    })
                    .is_err()
                {
                    break;
                }
                this.wake.notify_one();
                tokio::spawn(pump(stream, local, this.wake.clone()));
            }
        });
    }
}

/// Copy bytes between a local TCP stream and the netstack.
async fn pump(stream: TcpStream, local: LocalSide, wake: Arc<Notify>) {
    let (mut r, mut w) = stream.into_split();
    let LocalSide {
        to_net,
        mut from_net,
    } = local;
    let up = {
        let wake = wake.clone();
        async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match r.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if to_net.send(buf[..n].to_vec()).await.is_err() {
                            break;
                        }
                        wake.notify_one();
                    }
                }
            }
            drop(to_net);
            wake.notify_one();
        }
    };
    let down = async move {
        while let Some(chunk) = from_net.recv().await {
            wake.notify_one(); // channel space freed: the netstack may read more
            if w.write_all(&chunk).await.is_err() {
                break;
            }
        }
        let _ = w.shutdown().await;
    };
    tokio::join!(up, down);
}

/// IP packets between WireGuard and smoltcp.
#[derive(Default)]
struct Queues {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct RxTok(Vec<u8>);
struct TxTok<'a>(&'a mut VecDeque<Vec<u8>>);

impl phy::RxToken for RxTok {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(&self.0)
    }
}

impl phy::TxToken for TxTok<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

impl Device for Queues {
    type RxToken<'a> = RxTok;
    type TxToken<'a> = TxTok<'a>;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut c = DeviceCapabilities::default();
        c.medium = Medium::Ip;
        c.max_transmission_unit = MTU;
        c
    }

    fn receive(&mut self, _: SmolInstant) -> Option<(RxTok, TxTok<'_>)> {
        let p = self.rx.pop_front()?;
        Some((RxTok(p), TxTok(&mut self.tx)))
    }

    fn transmit(&mut self, _: SmolInstant) -> Option<TxTok<'_>> {
        Some(TxTok(&mut self.tx))
    }
}

struct Conn {
    handle: SocketHandle,
    net: NetSide,
    /// Bytes from the local side not yet accepted by the TCP socket.
    pending: Vec<u8>,
    local_closed: bool,
    from_net_open: bool,
}

pub struct NetstackConfig {
    pub private: [u8; 32],
    pub ip: Ipv4Addr,
    pub peers: Vec<PeerConfig>,
    /// Accept TCP on this tunnel port and forward each connection to a local address.
    pub accept: Option<(u16, SocketAddr)>,
}

struct Stack {
    udp: Arc<UdpSocket>,
    private: StaticSecret,
    public: PublicKey,
    peers: Vec<Peer>,
    iface: Interface,
    dev: Queues,
    sockets: SocketSet<'static>,
    conns: Vec<Conn>,
    listeners: Vec<SocketHandle>,
    accept: Option<(u16, SocketAddr)>,
    wake: Arc<Notify>,
    started: Instant,
    next_port: u16,
}

pub fn spawn(udp: UdpSocket, cfg: NetstackConfig) -> Netstack {
    let (cmds_tx, cmds_rx) = mpsc::unbounded_channel();
    let wake = Arc::new(Notify::new());
    let mut dev = Queues::default();
    let mut iface_cfg = Config::new(HardwareAddress::Ip);
    iface_cfg.random_seed = u64::from_le_bytes(new_private_key()[..8].try_into().unwrap());
    let started = Instant::now();
    let mut iface = Interface::new(iface_cfg, &mut dev, SmolInstant::from_millis(0));
    iface.update_ip_addrs(|a| {
        let _ = a.push(IpCidr::new(IpAddress::Ipv4(cfg.ip), 16));
    });
    let private = StaticSecret::from(cfg.private);
    let public = PublicKey::from(&private);
    let mut stack = Stack {
        udp: Arc::new(udp),
        private,
        public,
        peers: Vec::new(),
        iface,
        dev,
        sockets: SocketSet::new(Vec::new()),
        conns: Vec::new(),
        listeners: Vec::new(),
        accept: cfg.accept,
        wake: wake.clone(),
        started,
        next_port: 40000,
    };
    for p in cfg.peers {
        stack.add_peer(p);
    }
    tokio::spawn(stack.run(cmds_rx));
    Netstack {
        cmds: cmds_tx,
        wake,
    }
}

fn new_tcp_socket() -> tcp::Socket<'static> {
    let mut s = tcp::Socket::new(
        tcp::SocketBuffer::new(vec![0; TCP_BUF]),
        tcp::SocketBuffer::new(vec![0; TCP_BUF]),
    );
    s.set_nagle_enabled(false);
    s.set_keep_alive(Some(smoltcp::time::Duration::from_secs(30)));
    s.set_timeout(Some(smoltcp::time::Duration::from_secs(120)));
    s
}

impl Stack {
    fn now(&self) -> SmolInstant {
        SmolInstant::from_millis(self.started.elapsed().as_millis() as i64)
    }

    fn add_peer(&mut self, cfg: PeerConfig) {
        self.peers
            .retain(|p| p.cfg.public != cfg.public && p.cfg.ip != cfg.ip);
        let tunn = Tunn::new(
            self.private.clone(),
            PublicKey::from(cfg.public),
            None,
            cfg.keepalive,
            cfg.index,
            None,
        );
        self.peers.push(Peer {
            endpoint: cfg.endpoint,
            cfg,
            tunn,
        });
    }

    async fn run(mut self, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
        let mut buf = vec![0u8; 65536];
        let mut timers = tokio::time::interval(Duration::from_millis(250));
        loop {
            while let Ok(cmd) = cmds.try_recv() {
                self.command(cmd);
            }
            self.ensure_listeners();
            // Datagrams that already arrived.
            while let Ok((n, from)) = self.udp.try_recv_from(&mut buf) {
                let datagram = buf[..n].to_vec();
                self.datagram(&datagram, from).await;
            }
            self.pump_stack().await;

            let delay = self
                .iface
                .poll_delay(self.now(), &self.sockets)
                .map(|d| Duration::from_millis(d.total_millis()))
                .unwrap_or(Duration::from_millis(500))
                .min(Duration::from_millis(500));
            let udp = self.udp.clone();
            tokio::select! {
                r = udp.recv_from(&mut buf) => {
                    if let Ok((n, from)) = r {
                        let datagram = buf[..n].to_vec();
                        self.datagram(&datagram, from).await;
                    }
                }
                cmd = cmds.recv() => match cmd {
                    Some(c) => self.command(c),
                    None => return,
                },
                _ = self.wake.notified() => {}
                _ = timers.tick() => self.update_timers().await,
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::AddPeer(p) => self.add_peer(p),
            Cmd::RemovePeer(public) => self.peers.retain(|p| p.cfg.public != public),
            Cmd::Connect { remote, bridge } => {
                let mut sock = new_tcp_socket();
                self.next_port = if self.next_port >= 65000 {
                    40000
                } else {
                    self.next_port + 1
                };
                let remote = IpEndpoint::new(IpAddress::Ipv4(remote.0), remote.1);
                if sock
                    .connect(self.iface.context(), remote, self.next_port)
                    .is_err()
                {
                    return; // dropping the bridge closes the local stream
                }
                let handle = self.sockets.add(sock);
                self.conns.push(Conn {
                    handle,
                    net: bridge,
                    pending: Vec::new(),
                    local_closed: false,
                    from_net_open: true,
                });
            }
        }
    }

    fn ensure_listeners(&mut self) {
        let Some((port, _)) = self.accept else { return };
        while self.listeners.len() < LISTEN_BACKLOG {
            let mut s = new_tcp_socket();
            if s.listen(port).is_err() {
                return;
            }
            self.listeners.push(self.sockets.add(s));
        }
    }

    /// Move listeners that got a connection into `conns` and connect them to the local target.
    fn accept_new(&mut self) {
        let Some((_, target)) = self.accept else {
            return;
        };
        let mut i = 0;
        while i < self.listeners.len() {
            let h = self.listeners[i];
            if self.sockets.get::<tcp::Socket>(h).state() == tcp::State::Listen {
                i += 1;
                continue;
            }
            self.listeners.swap_remove(i);
            let (net, local) = bridge_pair();
            self.conns.push(Conn {
                handle: h,
                net,
                pending: Vec::new(),
                local_closed: false,
                from_net_open: true,
            });
            let wake = self.wake.clone();
            tokio::spawn(async move {
                if let Ok(stream) = TcpStream::connect(target).await {
                    let _ = stream.set_nodelay(true);
                    pump(stream, local, wake).await;
                }
            });
        }
    }

    /// Copy data between TCP sockets and their bridges; returns true if anything moved.
    fn service_conns(&mut self) -> bool {
        let mut moved = false;
        let mut buf = vec![0u8; 64 * 1024];
        self.conns.retain_mut(|c| {
            let s = self.sockets.get_mut::<tcp::Socket>(c.handle);
            // Local -> network.
            loop {
                if c.pending.is_empty() && !c.local_closed {
                    match c.net.to_net.try_recv() {
                        Ok(d) => c.pending = d,
                        Err(mpsc::error::TryRecvError::Empty) => {}
                        Err(mpsc::error::TryRecvError::Disconnected) => c.local_closed = true,
                    }
                }
                if c.pending.is_empty() || !s.can_send() {
                    break;
                }
                match s.send_slice(&c.pending) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        c.pending.drain(..n);
                        moved = true;
                    }
                }
            }
            if c.local_closed && c.pending.is_empty() && s.may_send() {
                s.close();
            }
            // Network -> local, only while the bridge has room (TCP window does backpressure).
            while c.from_net_open && s.can_recv() {
                let permit = match c.net.from_net.try_reserve() {
                    Ok(p) => p,
                    Err(mpsc::error::TrySendError::Full(_)) => break,
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        c.from_net_open = false;
                        s.abort();
                        break;
                    }
                };
                match s.recv_slice(&mut buf) {
                    Ok(n) if n > 0 => {
                        permit.send(buf[..n].to_vec());
                        moved = true;
                    }
                    _ => break,
                }
            }
            if c.from_net_open
                && !s.may_recv()
                && s.state() != tcp::State::SynSent
                && s.state() != tcp::State::SynReceived
            {
                // Peer finished sending: closing our sender ends the local stream's writes.
                c.from_net_open = false;
                let (tx, _) = mpsc::channel(1);
                c.net.from_net = tx;
            }
            let done = matches!(s.state(), tcp::State::Closed | tcp::State::TimeWait);
            if done {
                self.sockets.remove(c.handle);
            }
            !done
        });
        moved
    }

    async fn pump_stack(&mut self) {
        for _ in 0..64 {
            let now = self.now();
            self.iface.poll(now, &mut self.dev, &mut self.sockets);
            self.accept_new();
            let moved = self.service_conns();
            let now = self.now();
            self.iface.poll(now, &mut self.dev, &mut self.sockets);
            let sent = !self.dev.tx.is_empty();
            self.flush_tx().await;
            if !moved && !sent && self.dev.rx.is_empty() {
                break;
            }
        }
    }

    async fn flush_tx(&mut self) {
        let mut out = vec![0u8; 65536];
        while let Some(pkt) = self.dev.tx.pop_front() {
            if pkt.len() < 20 {
                continue;
            }
            let dst = Ipv4Addr::new(pkt[16], pkt[17], pkt[18], pkt[19]);
            let Some(peer) = self.peers.iter_mut().find(|p| p.cfg.ip == dst) else {
                continue;
            };
            if let TunnResult::WriteToNetwork(d) = peer.tunn.encapsulate(&pkt, &mut out)
                && let Some(ep) = peer.endpoint
            {
                let _ = self.udp.send_to(d, ep).await;
            }
        }
    }

    async fn update_timers(&mut self) {
        let mut out = vec![0u8; 2048];
        for p in &mut self.peers {
            if let TunnResult::WriteToNetwork(d) = p.tunn.update_timers(&mut out)
                && let Some(ep) = p.endpoint
            {
                let _ = self.udp.send_to(d, ep).await;
            }
        }
    }

    fn peer_for(&self, datagram: &[u8]) -> Option<usize> {
        let idx = match Tunn::parse_incoming_packet(datagram).ok()? {
            Packet::HandshakeInit(h) => {
                let half = parse_handshake_anon(&self.private, &self.public, &h).ok()?;
                return self
                    .peers
                    .iter()
                    .position(|p| p.cfg.public == half.peer_static_public);
            }
            Packet::HandshakeResponse(h) => h.receiver_idx,
            Packet::PacketCookieReply(c) => c.receiver_idx,
            Packet::PacketData(d) => d.receiver_idx,
        };
        self.peers.iter().position(|p| p.cfg.index == idx >> 8)
    }

    async fn datagram(&mut self, datagram: &[u8], from: SocketAddr) {
        let Some(i) = self.peer_for(datagram) else {
            return;
        };
        let mut out = vec![0u8; 65536];
        let peer = &mut self.peers[i];
        let mut accepted = false;
        let mut input: &[u8] = datagram;
        loop {
            match peer.tunn.decapsulate(Some(from.ip()), input, &mut out) {
                TunnResult::WriteToNetwork(d) => {
                    accepted = true;
                    let _ = self.udp.send_to(d, from).await;
                    input = &[]; // flush queued packets
                    continue;
                }
                TunnResult::WriteToTunnelV4(d, src) => {
                    accepted = true;
                    // Only accept packets from the peer's own tunnel address.
                    if src == peer.cfg.ip {
                        self.dev.rx.push_back(d.to_vec());
                    }
                }
                TunnResult::Done => accepted = true,
                TunnResult::WriteToTunnelV6(..) | TunnResult::Err(_) => {}
            }
            break;
        }
        if accepted {
            peer.endpoint = Some(from); // roaming: reply where the peer is now
        }
    }
}

/// Bind the UDP socket for a netstack.
/// Large kernel buffers matter: a TCP window's worth of packets arrives in one burst.
pub async fn bind_udp(addr: &str) -> Result<UdpSocket> {
    let sock = UdpSocket::bind(addr)
        .await
        .with_context(|| format!("bind UDP {addr}"))?;
    let s = socket2::SockRef::from(&sock);
    let _ = s.set_recv_buffer_size(8 << 20);
    let _ = s.set_send_buffer_size(8 << 20);
    Ok(sock)
}

/// Resolve `host:port` to a socket address (first result).
pub async fn resolve(endpoint: &str) -> Result<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(endpoint)
        .await
        .with_context(|| format!("resolve {endpoint}"))?
        .collect();
    addrs
        .into_iter()
        .find(|a| a.is_ipv4())
        .or(None)
        .ok_or_else(|| anyhow!("{endpoint} has no IPv4 address"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn join_code_round_trip() {
        let j = JoinCode {
            endpoint: "build.example.com:51820".into(),
            server_public: b64(&public_key(&new_private_key())),
            private: b64(&new_private_key()),
            ip: agent_ip(7),
            token: "mlk_x".into(),
        };
        assert_eq!(JoinCode::decode(&j.encode()).unwrap(), j);
        assert!(JoinCode::decode("nope").is_err());
        assert_eq!(agent_ip(1), Ipv4Addr::new(10, 77, 0, 2));
        assert_eq!(agent_ip(300), Ipv4Addr::new(10, 77, 1, 45));
    }

    /// Server and agent netstacks over real UDP on localhost: a local port on the agent
    /// reaches a TCP echo server behind the server's tunnel address, in both directions,
    /// with enough data to exercise flow control.
    #[tokio::test]
    async fn tcp_through_tunnel() {
        // Echo server standing in for moloko's HTTP server.
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = echo.accept().await {
                tokio::spawn(async move {
                    let (mut r, mut w) = s.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });

        let server_key = new_private_key();
        let agent_key = new_private_key();
        let server_udp = bind_udp("127.0.0.1:0").await.unwrap();
        let server_ep = server_udp.local_addr().unwrap();
        let _server = spawn(
            server_udp,
            NetstackConfig {
                private: server_key,
                ip: SERVER_IP,
                peers: vec![PeerConfig {
                    index: 5,
                    public: public_key(&agent_key),
                    ip: agent_ip(5),
                    endpoint: None,
                    keepalive: None,
                }],
                accept: Some((TUNNEL_PORT, echo_addr)),
            },
        );
        let agent = spawn(
            bind_udp("127.0.0.1:0").await.unwrap(),
            NetstackConfig {
                private: agent_key,
                ip: agent_ip(5),
                peers: vec![PeerConfig {
                    index: 1,
                    public: public_key(&server_key),
                    ip: SERVER_IP,
                    endpoint: Some(server_ep),
                    keepalive: Some(25),
                }],
                accept: None,
            },
        );
        let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = local.local_addr().unwrap();
        agent.forward_local(local, (SERVER_IP, TUNNEL_PORT));

        // Two concurrent connections, 3 MB each way.
        let mut tasks = Vec::new();
        for seed in 0..2u8 {
            tasks.push(tokio::spawn(async move {
                let payload: Vec<u8> = (0..3_000_000u32)
                    .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
                    .collect();
                let s = TcpStream::connect(local_addr).await.unwrap();
                let (mut r, mut w) = s.into_split();
                let p = payload.clone();
                let writer = tokio::spawn(async move {
                    w.write_all(&p).await.unwrap();
                    w.shutdown().await.unwrap();
                });
                let mut got = Vec::new();
                tokio::time::timeout(Duration::from_secs(30), r.read_to_end(&mut got))
                    .await
                    .expect("echo timed out")
                    .unwrap();
                writer.await.unwrap();
                assert_eq!(got.len(), payload.len());
                assert!(got == payload, "echoed data differs");
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
    }

    #[tokio::test]
    async fn unknown_key_is_ignored() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_addr = echo.local_addr().unwrap();
        let server_key = new_private_key();
        let server_udp = bind_udp("127.0.0.1:0").await.unwrap();
        let server_ep = server_udp.local_addr().unwrap();
        let _server = spawn(
            server_udp,
            NetstackConfig {
                private: server_key,
                ip: SERVER_IP,
                peers: vec![],
                accept: Some((TUNNEL_PORT, echo_addr)),
            },
        );
        let stranger = spawn(
            bind_udp("127.0.0.1:0").await.unwrap(),
            NetstackConfig {
                private: new_private_key(),
                ip: agent_ip(9),
                peers: vec![PeerConfig {
                    index: 1,
                    public: public_key(&server_key),
                    ip: SERVER_IP,
                    endpoint: Some(server_ep),
                    keepalive: None,
                }],
                accept: None,
            },
        );
        let local = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = local.local_addr().unwrap();
        stranger.forward_local(local, (SERVER_IP, TUNNEL_PORT));
        let accepted = tokio::time::timeout(Duration::from_secs(2), async {
            let mut s = TcpStream::connect(local_addr).await.unwrap();
            let _ = s.write_all(b"hi").await;
            echo.accept().await
        })
        .await;
        assert!(
            accepted.is_err(),
            "a peer with an unknown key must not reach the server"
        );
    }
}
