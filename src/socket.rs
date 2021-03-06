use std::cmp::{min, max};
use std::collections::{LinkedList, VecDeque};
use std::old_io::net::ip::SocketAddr;
use std::old_io::net::udp::UdpSocket;
use std::old_io::{IoResult, IoError, TimedOut, ConnectionFailed, EndOfFile, Closed, ConnectionReset};
use std::iter::{range_inclusive, repeat};
use std::num::SignedInt;
use util::{now_microseconds, ewma};
use packet::{Packet, PacketType, ExtensionType, HEADER_SIZE};
use rand;

// For simplicity's sake, let us assume no packet will ever exceed the
// Ethernet maximum transfer unit of 1500 bytes.
const BUF_SIZE: usize = 1500;
const GAIN: f64 = 1.0;
const ALLOWED_INCREASE: u32 = 1;
const TARGET: i64 = 100_000; // 100 milliseconds
const MSS: u32 = 1400;
const MIN_CWND: u32 = 2;
const INIT_CWND: u32 = 2;
const INITIAL_CONGESTION_TIMEOUT: u64 = 1000; // one second
const MIN_CONGESTION_TIMEOUT: u64 = 500; // 500 ms
const MAX_CONGESTION_TIMEOUT: u64 = 60_000; // one minute
const BASE_HISTORY: usize = 10; // base delays history size

macro_rules! iotry {
    ($e:expr) => (match $e { Ok(e) => e, Err(e) => panic!("{}", e) })
}

#[derive(PartialEq,Eq,Debug,Copy)]
enum SocketState {
    New,
    Connected,
    SynSent,
    FinReceived,
    FinSent,
    ResetReceived,
    Closed,
}

type TimestampSender = i64;
type TimestampReceived = i64;

struct DelaySample {
    received_at: TimestampReceived,
    sent_at: TimestampSender,
}

struct DelayDifferenceSample {
    received_at: TimestampReceived,
    difference: TimestampSender,
}

/// A uTP (Micro Transport Protocol) socket.
pub struct UtpSocket {
    /// The wrapped UDP socket
    socket: UdpSocket,
    /// Remote peer
    connected_to: SocketAddr,
    /// Sender connection identifier
    sender_connection_id: u16,
    /// Receiver connection identifier
    receiver_connection_id: u16,
    /// Sequence number for the next packet
    seq_nr: u16,
    /// Sequence number of the latest acknowledged packet sent by the remote peer
    ack_nr: u16,
    /// Socket state
    state: SocketState,
    /// Received but not acknowledged packets
    incoming_buffer: Vec<Packet>,
    /// Sent but not yet acknowledged packets
    send_window: Vec<Packet>,
    /// Packets not yet sent
    unsent_queue: LinkedList<Packet>,
    /// How many ACKs did the socket receive for packet with sequence number equal to `ack_nr`
    duplicate_ack_count: u32,
    /// Sequence number of the latest packet the remote peer acknowledged
    last_acked: u16,
    /// Timestamp of the latest packet the remote peer acknowledged
    last_acked_timestamp: u32,
    /// Sequence number of the received FIN packet, if any
    fin_seq_nr: u16,
    /// Round-trip time to remote peer
    rtt: i32,
    /// Variance of the round-trip time to the remote peer
    rtt_variance: i32,
    /// Data from the latest packet not yet returned in `recv_from`
    pending_data: Vec<u8>,
    /// Bytes in flight
    curr_window: u32,
    /// Window size of the remote peer
    remote_wnd_size: u32,
    /// Rolling window of packet delay to remote peer
    base_delays: VecDeque<DelaySample>,
    /// Rolling window of the difference between sending a packet and receiving its acknowledgement
    current_delays: Vec<DelayDifferenceSample>,
    /// Current congestion timeout in milliseconds
    congestion_timeout: u64,
    /// Congestion window in bytes
    cwnd: u32,
}

impl UtpSocket {
    /// Create a UTP socket from the given address.
    #[unstable]
    pub fn bind(addr: SocketAddr) -> IoResult<UtpSocket> {
        let skt = UdpSocket::bind(addr);
        let connection_id = rand::random::<u16>();
        match skt {
            Ok(x) => Ok(UtpSocket {
                socket: x,
                connected_to: addr,
                receiver_connection_id: connection_id,
                sender_connection_id: connection_id + 1,
                seq_nr: 1,
                ack_nr: 0,
                state: SocketState::New,
                incoming_buffer: Vec::new(),
                send_window: Vec::new(),
                unsent_queue: LinkedList::new(),
                duplicate_ack_count: 0,
                last_acked: 0,
                last_acked_timestamp: 0,
                fin_seq_nr: 0,
                rtt: 0,
                rtt_variance: 0,
                pending_data: Vec::new(),
                curr_window: 0,
                remote_wnd_size: 0,
                current_delays: Vec::new(),
                base_delays: VecDeque::with_capacity(BASE_HISTORY),
                congestion_timeout: INITIAL_CONGESTION_TIMEOUT,
                cwnd: INIT_CWND * MSS,
            }),
            Err(e) => Err(e)
        }
    }

    /// Open a uTP connection to a remote host by hostname or IP address.
    #[unstable]
    pub fn connect(mut self, other: SocketAddr) -> IoResult<UtpSocket> {
        self.connected_to = other;
        assert_eq!(self.receiver_connection_id + 1, self.sender_connection_id);

        let mut packet = Packet::new();
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(self.receiver_connection_id);
        packet.set_seq_nr(self.seq_nr);

        let mut len = 0;
        let mut addr = self.connected_to;
        let mut buf = [0; BUF_SIZE];

        let mut syn_timeout = self.congestion_timeout;
        for _ in (0u8..5) {
            packet.set_timestamp_microseconds(now_microseconds());

            // Send packet
            debug!("Connecting to {}", other);
            try!(self.socket.send_to(&packet.bytes()[..], other));
            self.state = SocketState::SynSent;

            // Validate response
            self.socket.set_read_timeout(Some(syn_timeout));
            match self.socket.recv_from(&mut buf) {
                Ok((read, src)) => { len = read; addr = src; break; },
                Err(ref e) if e.kind == TimedOut => {
                    debug!("Timed out, retrying");
                    syn_timeout *= 2;
                    continue;
                },
                Err(e) => return Err(e),
            };
        }
        assert!(len == HEADER_SIZE);
        assert!(addr == self.connected_to);

        let packet = Packet::decode(&buf[..len]);
        if packet.get_type() != PacketType::State {
            return Err(IoError {
                kind: ConnectionFailed,
                desc: "The remote peer sent an invalid reply",
                detail: None,
            });
        }
        try!(self.handle_packet(&packet, addr));

        debug!("connected to: {}", self.connected_to);

        return Ok(self);
    }

    /// Gracefully close connection to peer.
    ///
    /// This method allows both peers to receive all packets still in
    /// flight.
    #[unstable]
    pub fn close(&mut self) -> IoResult<()> {
        // Wait for acknowledgment on pending sent packets
        let mut buf = [0u8; BUF_SIZE];
        while !self.send_window.is_empty() {
            try!(self.recv_from(&mut buf));
        }

        // Nothing to do if the socket's already closed
        if self.state == SocketState::Closed {
            return Ok(());
        }

        let mut packet = Packet::new();
        packet.set_connection_id(self.sender_connection_id);
        packet.set_seq_nr(self.seq_nr);
        packet.set_ack_nr(self.ack_nr);
        packet.set_timestamp_microseconds(now_microseconds());
        packet.set_type(PacketType::Fin);

        // Send FIN
        try!(self.socket.send_to(&packet.bytes()[..], self.connected_to));
        self.state = SocketState::FinSent;

        // Receive JAKE
        while self.state != SocketState::Closed {
            try!(self.recv_from(&mut buf));
        }

        Ok(())
    }

    /// Receive data from socket.
    ///
    /// On success, returns the number of bytes read and the sender's address.
    /// Returns `Closed` after receiving a FIN packet when the remaining
    /// inflight packets are consumed.
    #[unstable]
    pub fn recv_from(&mut self, buf: &mut[u8]) -> IoResult<(usize,SocketAddr)> {
        if self.state == SocketState::Closed {
            return Err(IoError {
                kind: EndOfFile,
                desc: "End of file reached",
                detail: None,
            });
        }

        if self.state == SocketState::ResetReceived {
            return Err(IoError {
                kind: Closed,
                desc: "Connection reset",
                detail: None,
            });
        }

        match self.flush_incoming_buffer(buf) {
            0 => self.recv(buf),
            read => Ok((read, self.connected_to)),
        }
    }

    fn recv(&mut self, buf: &mut[u8]) -> IoResult<(usize,SocketAddr)> {
        let mut b = [0; BUF_SIZE + HEADER_SIZE];
        if self.state != SocketState::New {
            debug!("setting read timeout of {} ms", self.congestion_timeout);
            self.socket.set_read_timeout(Some(self.congestion_timeout));
        }
        let (read, src) = match self.socket.recv_from(&mut b) {
            Err(ref e) if e.kind == TimedOut => {
                debug!("recv_from timed out");
                self.congestion_timeout = self.congestion_timeout * 2;
                self.cwnd = MSS;
                self.send_fast_resend_request();
                return Ok((0, self.connected_to));
            },
            Ok(x) => x,
            Err(e) => return Err(e),
        };
        let packet = Packet::decode(&b[..read]);
        debug!("received {:?}", packet);

        let shallow_clone = packet.shallow_clone();

        if packet.get_type() == PacketType::Data && self.ack_nr.wrapping_add(1) <= packet.seq_nr() {
            self.insert_into_buffer(packet);
        }

        if let Some(pkt) = try!(self.handle_packet(&shallow_clone, src)) {
                let mut pkt = pkt;
                pkt.set_wnd_size(BUF_SIZE as u32);
                try!(self.socket.send_to(&pkt.bytes()[..], src));
                debug!("sent {:?}", pkt);
        }

        // Flush incoming buffer if possible
        let read = self.flush_incoming_buffer(buf);

        Ok((read, src))
    }

    fn prepare_reply(&self, original: &Packet, t: PacketType) -> Packet {
        let mut resp = Packet::new();
        resp.set_type(t);
        let self_t_micro: u32 = now_microseconds();
        let other_t_micro: u32 = original.timestamp_microseconds();
        resp.set_timestamp_microseconds(self_t_micro);
        resp.set_timestamp_difference_microseconds((self_t_micro - other_t_micro));
        resp.set_connection_id(self.sender_connection_id);
        resp.set_seq_nr(self.seq_nr);
        resp.set_ack_nr(self.ack_nr);

        resp
    }

    /// Remove packet in incoming buffer and update current acknowledgement
    /// number.
    fn advance_incoming_buffer(&mut self) -> Option<Packet> {
        if !self.incoming_buffer.is_empty() {
            let packet = self.incoming_buffer.remove(0);
            debug!("Removed packet from incoming buffer: {:?}", packet);
            self.ack_nr = packet.seq_nr();
            Some(packet)
        } else {
            None
        }
    }

    /// Discards sequential, ordered packets in incoming buffer, starting from
    /// the most recently acknowledged to the most recent, as long as there are
    /// no missing packets. The discarded packets' payload is written to the
    /// slice `buf`, starting in position `start`.
    /// Returns the last written index.
    fn flush_incoming_buffer(&mut self, buf: &mut [u8]) -> usize {
        let mut idx = 0;

        // Check if there is any pending data from a partially flushed packet
        if !self.pending_data.is_empty() {
            let len = buf.clone_from_slice(&self.pending_data[..]);

            // If all the data in the pending data buffer fits the given output
            // buffer, remove the corresponding packet from the incoming buffer
            // and clear the pending data buffer
            if len == self.pending_data.len() {
                self.pending_data.clear();
                self.advance_incoming_buffer();
                return idx + len;
            } else {
                // Remove the bytes copied to the output buffer from the pending
                // data buffer (i.e., pending -= output)
                self.pending_data = self.pending_data[len..].to_vec();
            }
        }

        // Copy the payload of as many packets in the incoming buffer as possible
        while !self.incoming_buffer.is_empty() &&
            (self.ack_nr == self.incoming_buffer[0].seq_nr() ||
             self.ack_nr + 1 == self.incoming_buffer[0].seq_nr())
        {
            let len = min(buf.len() - idx, self.incoming_buffer[0].payload.len());

            for i in (0..len) {
                buf[idx] = self.incoming_buffer[0].payload[i];
                idx += 1;
            }

            // Remove top packet if its payload fits the output buffer
            if self.incoming_buffer[0].payload.len() == len {
                self.advance_incoming_buffer();
            } else {
                self.pending_data.push_all(&self.incoming_buffer[0].payload[len..]);
            }

            // Stop if the output buffer is full
            if buf.len() == idx {
                return idx;
            }
        }

        return idx;
    }

    /// Send data on socket to the remote peer. Returns nothing on success.
    //
    // # Implementation details
    //
    // This method inserts packets into the send buffer and keeps trying to
    // advance the send window until an ACK corresponding to the last packet is
    // received.
    //
    // Note that the buffer passed to `send_to` might exceed the maximum packet
    // size, which will result in the data being split over several packets.
    #[unstable]
    pub fn send_to(&mut self, buf: &[u8]) -> IoResult<()> {
        if self.state == SocketState::Closed {
            return Err(IoError {
                kind: Closed,
                desc: "Connection closed",
                detail: None,
            });
        }

        for chunk in buf.chunks(MSS as usize - HEADER_SIZE) {
            let mut packet = Packet::new();
            packet.set_type(PacketType::Data);
            packet.payload = chunk.to_vec();
            packet.set_seq_nr(self.seq_nr);
            packet.set_ack_nr(self.ack_nr);
            packet.set_connection_id(self.sender_connection_id);

            self.unsent_queue.push_back(packet);
            if self.seq_nr == ::std::u16::MAX {
                self.seq_nr = 0;
            } else {
                self.seq_nr += 1;
            }
        }

        // Flush unsent packet queue
        try!(self.send());

        // Consume acknowledgements until latest packet
        let mut buf = [0; BUF_SIZE];
        while self.last_acked < self.seq_nr - 1 {
            try!(self.recv_from(&mut buf));
        }

        Ok(())
    }

    /// Send every packet in the unsent packet queue.
    fn send(&mut self) -> IoResult<()> {
        let dst = self.connected_to;
        while let Some(packet) = self.unsent_queue.pop_front() {
            debug!("current window: {}", self.send_window.len());
            let max_inflight = min(self.cwnd, self.remote_wnd_size);
            let max_inflight = max(MIN_CWND * MSS, max_inflight);
            while self.curr_window + packet.len() as u32 > max_inflight {
                let mut buf = [0; BUF_SIZE];
                iotry!(self.recv_from(&mut buf));
            }

            let mut packet = packet;
            packet.set_timestamp_microseconds(now_microseconds());
            try!(self.socket.send_to(&packet.bytes()[..], dst));
            debug!("sent {:?}", packet);
            self.curr_window += packet.len() as u32;
            self.send_window.push(packet);
        }
        Ok(())
    }

    /// Send fast resend request.
    ///
    /// Sends three identical ACK/STATE packets to the remote host, signalling a
    /// fast resend request.
    fn send_fast_resend_request(&mut self) {
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::State);
        packet.set_ack_nr(self.ack_nr);
        packet.set_seq_nr(self.seq_nr);
        packet.set_connection_id(self.sender_connection_id);

        for _ in (0u8..3) {
            let t = now_microseconds();
            packet.set_timestamp_microseconds(t);
            packet.set_timestamp_difference_microseconds((t - self.last_acked_timestamp));
            iotry!(self.socket.send_to(&packet.bytes()[..], self.connected_to));
            debug!("sent {:?}", packet);
        }
    }

    fn update_base_delay(&mut self, v: i64, now: i64) {
        use std::num::Int;
        let minute_in_microseconds = 60 * 10.pow(6);

        if self.base_delays.is_empty() || now - self.base_delays[0].received_at > minute_in_microseconds {
            // Drop the oldest sample and save minimum for current minute
            if self.base_delays.len() == BASE_HISTORY {
                self.base_delays.pop_back();
            }
            self.base_delays.push_front(DelaySample{ received_at: now, sent_at: v });
        } else {
            // Replace sample for the current minute if the delay is lower
            if v < self.base_delays[0].sent_at {
                self.base_delays[0] = DelaySample{ received_at: now, sent_at: v};
            }
        }
    }

    /// Insert a new sample in the current delay list after removing samples older than one RTT, as
    /// specified in RFC6817.
    fn update_current_delay(&mut self, v: i64, now: i64) {
        // Remove samples more than one RTT old
        let rtt = self.rtt as i64 * 100;
        while !self.current_delays.is_empty() && now - self.current_delays[0].received_at > rtt {
            self.current_delays.remove(0);
        }

        // Insert new measurement
        self.current_delays.push(DelayDifferenceSample{ received_at: now, difference: v });
    }

    fn update_congestion_timeout(&mut self, current_delay: i32) {
        let delta = self.rtt - current_delay;
        self.rtt_variance += (delta.abs() - self.rtt_variance) / 4;
        self.rtt += (current_delay - self.rtt) / 8;
        self.congestion_timeout = max((self.rtt + self.rtt_variance * 4) as u64, MIN_CONGESTION_TIMEOUT);
        self.congestion_timeout = min(self.congestion_timeout, MAX_CONGESTION_TIMEOUT);

        debug!("current_delay: {}", current_delay);
        debug!("delta: {}", delta);
        debug!("self.rtt_variance: {}", self.rtt_variance);
        debug!("self.rtt: {}", self.rtt);
        debug!("self.congestion_timeout: {}", self.congestion_timeout);
    }

    /// Calculate the filtered current delay in the current window.
    ///
    /// The current delay is calculated through application of the exponential
    /// weighted moving average filter with smoothing factor 0.333 over the
    /// current delays in the current window.
    fn filtered_current_delay(&self) -> i64 {
        let input = self.current_delays.iter().map(|&ref x| x.difference).collect();
        ewma(input, 0.333) as i64
    }

    /// Calculate the lowest base delay in the current window.
    fn min_base_delay(&self) -> i64 {
        match self.base_delays.iter().min_by(|&x| (x.received_at - x.sent_at).abs()) {
            Some(ref x) => x.received_at - x.sent_at,
            None => 0
        }
    }

    /// Build the selective acknowledgment payload for usage in packets.
    fn build_selective_ack(&self) -> Vec<u8> {
        let stashed = self.incoming_buffer.iter()
            .filter(|&pkt| pkt.seq_nr() > self.ack_nr);

        let mut sack = Vec::new();
        for packet in stashed {
            let diff = packet.seq_nr() - self.ack_nr - 2;
            let byte = (diff / 8) as usize;
            let bit = (diff % 8) as usize;

            if byte >= sack.len() {
                sack.push(0u8);
            }

            let mut bitarray = sack.pop().unwrap();
            bitarray |= 1 << bit;
            sack.push(bitarray);
        }

        // Make sure the amount of elements in the SACK vector is a
        // multiple of 4
        if sack.len() % 4 != 0 {
            let len = sack.len();
            sack.extend(repeat(0).take((len / 4 + 1) * 4 - len));
        }

        return sack;
    }

    fn resend_lost_packet(&mut self, lost_packet_nr: u16) {
        match self.send_window.iter().find(|pkt| pkt.seq_nr() == lost_packet_nr) {
            None => debug!("Packet {} not found", lost_packet_nr),
            Some(packet) => {
                iotry!(self.socket.send_to(&packet.bytes()[..], self.connected_to));
                debug!("sent {:?}", packet);
            }
        }
    }

    /// Forget sent packets that were acknowledged by the remote peer.
    fn advance_send_window(&mut self) {
        if let Some(position) = self.send_window.iter()
            .position(|pkt| pkt.seq_nr() == self.last_acked)
        {
            for _ in range_inclusive(0, position) {
                let packet = self.send_window.remove(0);
                self.curr_window -= packet.len() as u32;
            }
        }
        debug!("self.curr_window: {}", self.curr_window);
    }

    /// Handle incoming packet, updating socket state accordingly.
    ///
    /// Returns appropriate reply packet, if needed.
    fn handle_packet(&mut self, packet: &Packet, src: SocketAddr) -> IoResult<Option<Packet>> {
        debug!("({:?}, {:?})", self.state, packet.get_type());

        // Acknowledge only if the packet strictly follows the previous one
        if packet.seq_nr().wrapping_sub(self.ack_nr) == 1 {
            self.ack_nr = packet.seq_nr();
        }

        // Reset connection if connection id doesn't match and this isn't a SYN
        if (self.state, packet.get_type()) != (SocketState::New, PacketType::Syn) &&
            !(packet.connection_id() == self.sender_connection_id ||
              packet.connection_id() == self.receiver_connection_id) {
            return Ok(Some(self.prepare_reply(packet, PacketType::Reset)));
        }

        self.remote_wnd_size = packet.wnd_size() as u32;
        debug!("self.remote_wnd_size: {}", self.remote_wnd_size);

        match (self.state, packet.get_type()) {
            (SocketState::New, PacketType::Syn) => {
                self.connected_to = src;
                self.ack_nr = packet.seq_nr();
                self.seq_nr = rand::random();
                self.receiver_connection_id = packet.connection_id() + 1;
                self.sender_connection_id = packet.connection_id();
                self.state = SocketState::Connected;

                Ok(Some(self.prepare_reply(packet, PacketType::State)))
            },
            (SocketState::SynSent, PacketType::State) => {
                self.ack_nr = packet.seq_nr();
                self.seq_nr += 1;
                self.state = SocketState::Connected;
                self.last_acked = packet.ack_nr();
                self.last_acked_timestamp = now_microseconds();
                Ok(None)
            },
            (SocketState::SynSent, _) => {
                Err(IoError {
                    kind: ConnectionFailed,
                    desc: "The remote peer sent an invalid reply",
                    detail: None,
                })
            }
            (SocketState::Connected, PacketType::Syn) => Ok(None), // ignore
            (SocketState::Connected, PacketType::Data) => {
                Ok(self.handle_data_packet(packet))
            },
            (SocketState::Connected, PacketType::State) => {
                self.handle_state_packet(packet);
                Ok(None)
            },
            (SocketState::Connected, PacketType::Fin) => {
                self.state = SocketState::FinReceived;
                self.fin_seq_nr = packet.seq_nr();

                // If all packets are received and handled
                if self.no_pending_data() && self.ack_nr == self.fin_seq_nr
                {
                    self.state = SocketState::Closed;
                    Ok(Some(self.prepare_reply(packet, PacketType::State)))
                } else {
                    debug!("FIN received but there are missing packets");
                    Ok(None)
                }
            }
            (SocketState::FinSent, PacketType::State) => {
                if packet.ack_nr() == self.seq_nr {
                    self.state = SocketState::Closed;
                }
                Ok(None)
            }
            (_, PacketType::Reset) => {
                self.state = SocketState::ResetReceived;
                Err(IoError {
                    kind: ConnectionReset,
                    desc: "Remote host aborted connection (incorrect connection id)",
                    detail: None,
                })
            },
            (state, ty) => panic!("Unimplemented handling for ({:?},{:?})", state, ty)
        }
    }

    fn handle_data_packet(&mut self, packet: &Packet) -> Option<Packet> {
        let mut reply = self.prepare_reply(packet, PacketType::State);

        if packet.seq_nr().wrapping_sub(self.ack_nr) > 1 {
            debug!("current ack_nr ({}) is behind received packet seq_nr ({})",
                   self.ack_nr, packet.seq_nr());

            // Set SACK extension payload if the packet is not in order
            let sack = self.build_selective_ack();

            if sack.len() > 0 {
                reply.set_sack(Some(sack));
            }
        }

        Some(reply)
    }

    fn queuing_delay(&self) -> i64 {
        let filtered_current_delay = self.filtered_current_delay();
        let min_base_delay = self.min_base_delay();
        let queuing_delay = filtered_current_delay.abs() - min_base_delay.abs();

        debug!("filtered_current_delay: {}", filtered_current_delay);
        debug!("min_base_delay: {}", min_base_delay);
        debug!("queuing_delay: {}", queuing_delay);

        return queuing_delay;
    }

    fn update_congestion_window(&mut self, off_target: f64, bytes_newly_acked: u32) {
        use std::num::Int;

        let flightsize = self.curr_window;
        match self.cwnd.checked_add((GAIN * off_target * bytes_newly_acked as f64 * MSS as f64 / self.cwnd as f64) as u32) {
            Some(_) => {
                let max_allowed_cwnd = flightsize + ALLOWED_INCREASE * MSS;
                self.cwnd = min(self.cwnd, max_allowed_cwnd);
                self.cwnd = max(self.cwnd, MIN_CWND * MSS);

                debug!("cwnd: {}", self.cwnd);
                debug!("max_allowed_cwnd: {}", max_allowed_cwnd);
            }
            None => {
                // FIXME: This shouldn't happen at all, more investigation is needed to ascertain the
                // true cause of the miscalculation of the congestion window increase. For now, we
                // simply ignore meaningly large increases.
            }
        }
    }

    fn handle_state_packet(&mut self, packet: &Packet) {
        if packet.ack_nr() == self.last_acked {
            self.duplicate_ack_count += 1;
        } else {
            self.last_acked = packet.ack_nr();
            self.last_acked_timestamp = now_microseconds();
            self.duplicate_ack_count = 1;
        }

        // Update base and current delay
        let now = now_microseconds() as i64;
        self.update_base_delay(packet.timestamp_microseconds() as i64, now);
        self.update_current_delay(packet.timestamp_difference_microseconds() as i64, now);

        let off_target: f64 = (TARGET as f64 - self.queuing_delay() as f64) / TARGET as f64;
        debug!("off_target: {}", off_target);

        // Update congestion window size
        self.update_congestion_window(off_target, packet.len() as u32);

        // Update congestion timeout
        let rtt = (TARGET - off_target as i64) / 1000; // in milliseconds
        self.update_congestion_timeout(rtt as i32);

        let mut packet_loss_detected: bool = !self.send_window.is_empty() &&
                                             self.duplicate_ack_count == 3;

        // Process extensions, if any
        for extension in packet.extensions.iter() {
            if extension.get_type() == ExtensionType::SelectiveAck {
                let bits = extension.iter();
                // If three or more packets are acknowledged past the implicit missing one,
                // assume it was lost.
                if bits.filter(|&bit| bit == 1).count() >= 3 {
                    self.resend_lost_packet(packet.ack_nr() + 1);
                    packet_loss_detected = true;
                }

                let bits = extension.iter();
                for (idx, received) in bits.map(|bit| bit == 1).enumerate() {
                    let seq_nr = packet.ack_nr() + 2 + idx as u16;
                    if received {
                        debug!("SACK: packet {} received", seq_nr);
                    } else if !self.send_window.is_empty() &&
                        seq_nr < self.send_window.last().unwrap().seq_nr()
                    {
                        debug!("SACK: packet {} lost", seq_nr);
                        self.resend_lost_packet(seq_nr);
                        packet_loss_detected = true;
                    } else {
                        break;
                    }
                }
            } else {
                debug!("Unknown extension {:?}, ignoring", extension.get_type());
            }
        }

        // Packet lost, halve the congestion window
        if packet_loss_detected {
            debug!("packet loss detected, halving congestion window");
            self.cwnd = max(self.cwnd / 2, MIN_CWND * MSS);
            debug!("cwnd: {}", self.cwnd);
        }

        // Three duplicate ACKs, must resend packets since `ack_nr + 1`
        // TODO: checking if the send buffer isn't empty isn't a
        // foolproof way to differentiate between triple-ACK and three
        // keep alives spread in time
        if !self.send_window.is_empty() && self.duplicate_ack_count == 3 {
            for i in (0..self.send_window.len()) {
                let seq_nr = self.send_window[i].seq_nr();
                if seq_nr <= packet.ack_nr() { continue; }
                self.resend_lost_packet(seq_nr);
            }
        }

        // Success, advance send window
        self.advance_send_window();
    }

    /// Insert a packet into the socket's buffer.
    ///
    /// The packet is inserted in such a way that the buffer is
    /// ordered ascendingly by their sequence number. This allows
    /// storing packets that were received out of order.
    ///
    /// Inserting a duplicate of a packet will replace the one in the buffer if
    /// it's more recent (larger timestamp).
    fn insert_into_buffer(&mut self, packet: Packet) {
        let mut i = 0;
        for pkt in self.incoming_buffer.iter() {
            if pkt.seq_nr() >= packet.seq_nr() {
                break;
            }
            i += 1;
        }

        if !self.incoming_buffer.is_empty() && i < self.incoming_buffer.len() &&
            self.incoming_buffer[i].seq_nr() == packet.seq_nr() {
            self.incoming_buffer.remove(i);
        }
        self.incoming_buffer.insert(i, packet);
    }

    /// Checks whether there is pending data (to be returned on a `recv_from` call) on the socket
    fn no_pending_data(&self) -> bool {
        self.pending_data.is_empty() && self.incoming_buffer.is_empty()
    }
}

#[cfg(test)]
mod test {
    use std::old_io::test::next_test_ip4;
    use std::old_io::{EndOfFile, Closed};
    use std::old_io::net::udp::UdpSocket;
    use std::thread;
    use super::{UtpSocket, SocketState, BUF_SIZE};
    use packet::{Packet, PacketType};
    use util::now_microseconds;
    use rand;

    #[test]
    fn test_socket_ipv4() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());

        let client = iotry!(UtpSocket::bind(client_addr));
        let mut server = iotry!(UtpSocket::bind(server_addr));

        assert!(server.state == SocketState::New);
        assert!(client.state == SocketState::New);

        // Check proper difference in client's send connection id and receive connection id
        assert_eq!(client.sender_connection_id, client.receiver_connection_id + 1);

        thread::spawn(move || {
            let client = iotry!(client.connect(server_addr));
            assert!(client.state == SocketState::Connected);
            assert_eq!(client.connected_to, server_addr);
            drop(client);
        });

        let mut buf = [0u8; BUF_SIZE];
        match server.recv_from(&mut buf) {
            e => println!("{:?}", e),
        }
        // After establishing a new connection, the server's ids are a mirror of the client's.
        assert_eq!(server.receiver_connection_id, server.sender_connection_id + 1);
        assert_eq!(server.connected_to, client_addr);

        assert!(server.state == SocketState::Connected);
        drop(server);
    }

    #[test]
    fn test_recvfrom_on_closed_socket() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());

        let client = iotry!(UtpSocket::bind(client_addr));
        let mut server = iotry!(UtpSocket::bind(server_addr));

        assert!(server.state == SocketState::New);
        assert!(client.state == SocketState::New);

        thread::spawn(move || {
            let mut client = iotry!(client.connect(server_addr));
            assert!(client.state == SocketState::Connected);
            assert_eq!(client.close(), Ok(()));
            drop(client);
        });

        // Make the server listen for incoming connections
        let mut buf = [0u8; BUF_SIZE];
        let _resp = server.recv_from(&mut buf);
        assert!(server.state == SocketState::Connected);

        // Closing the connection is fine
        match server.recv_from(&mut buf) {
            Err(e) => panic!("{}", e),
            _ => {},
        }
        assert_eq!(server.state, SocketState::Closed);

        // Trying to listen on the socket after closing it raises an
        // EOF error
        match server.recv_from(&mut buf) {
            Err(e) => assert_eq!(e.kind, EndOfFile),
            v => panic!("expected {:?}, got {:?}", EndOfFile, v),
        }

        assert_eq!(server.state, SocketState::Closed);

        // Trying again raises a EndOfFile error
        match server.recv_from(&mut buf) {
            Err(e) => assert_eq!(e.kind, EndOfFile),
            v => panic!("expected {:?}, got {:?}", EndOfFile, v),
        }

        drop(server);
    }

    #[test]
    fn test_sendto_on_closed_socket() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());

        let client = iotry!(UtpSocket::bind(client_addr));
        let mut server = iotry!(UtpSocket::bind(server_addr));

        assert!(server.state == SocketState::New);
        assert!(client.state == SocketState::New);

        thread::spawn(move || {
            let client = iotry!(client.connect(server_addr));
            assert!(client.state == SocketState::Connected);
            let mut buf = [0u8; BUF_SIZE];
            let mut client = client;
            iotry!(client.recv_from(&mut buf));
        });

        // Make the server listen for incoming connections
        let mut buf = [0u8; BUF_SIZE];
        let (_read, _src) = iotry!(server.recv_from(&mut buf));
        assert!(server.state == SocketState::Connected);

        iotry!(server.close());
        assert_eq!(server.state, SocketState::Closed);

        // Trying to send to the socket after closing it raises an
        // error
        match server.send_to(&buf) {
            Err(e) => assert_eq!(e.kind, Closed),
            v => panic!("expected {:?}, got {:?}", Closed, v),
        }

        drop(server);
    }

    #[test]
    fn test_acks_on_socket() {
        use std::sync::mpsc::channel;
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());
        let (tx, rx) = channel();

        let client = iotry!(UtpSocket::bind(client_addr));
        let server = iotry!(UtpSocket::bind(server_addr));

        thread::spawn(move || {
            // Make the server listen for incoming connections
            let mut server = server;
            let mut buf = [0u8; BUF_SIZE];
            let _resp = server.recv_from(&mut buf);
            tx.send(server.seq_nr).unwrap();

            // Close the connection
            iotry!(server.recv_from(&mut buf));

            drop(server);
        });

        let mut client = iotry!(client.connect(server_addr));
        assert!(client.state == SocketState::Connected);
        let sender_seq_nr = rx.recv().unwrap();
        let ack_nr = client.ack_nr;
        assert!(ack_nr != 0);
        assert!(ack_nr == sender_seq_nr);
        assert_eq!(client.close(), Ok(()));

        // The reply to both connect (SYN) and close (FIN) should be
        // STATE packets, which don't increase the sequence number
        // and, hence, the receiver's acknowledgement number.
        assert!(client.ack_nr == ack_nr);
        drop(client);
    }

    #[test]
    fn test_handle_packet() {
        //fn test_connection_setup() {
        let initial_connection_id: u16 = rand::random();
        let sender_connection_id = initial_connection_id + 1;
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());
        let mut socket = iotry!(UtpSocket::bind(server_addr));

        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(initial_connection_id);

        // Do we have a response?
        let response = socket.handle_packet(&packet, client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());

        // Is is of the correct type?
        let response = response.unwrap();
        assert!(response.get_type() == PacketType::State);

        // Same connection id on both ends during connection establishment
        assert!(response.connection_id() == packet.connection_id());

        // Response acknowledges SYN
        assert!(response.ack_nr() == packet.seq_nr());

        // No payload?
        assert!(response.payload.is_empty());
        //}

        // ---------------------------------

        // fn test_connection_usage() {
        let old_packet = packet;
        let old_response = response;

        let mut packet = Packet::new();
        packet.set_type(PacketType::Data);
        packet.set_connection_id(sender_connection_id);
        packet.set_seq_nr(old_packet.seq_nr() + 1);
        packet.set_ack_nr(old_response.seq_nr());

        let response = socket.handle_packet(&packet, client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());

        let response = response.unwrap();
        assert!(response.get_type() == PacketType::State);

        // Sender (i.e., who initated connection and sent SYN) has connection id
        // equal to initial connection id + 1
        // Receiver (i.e., who accepted connection) has connection id equal to
        // initial connection id
        assert!(response.connection_id() == initial_connection_id);
        assert!(response.connection_id() == packet.connection_id() - 1);

        // Previous packets should be ack'ed
        assert!(response.ack_nr() == packet.seq_nr());

        // Responses with no payload should not increase the sequence number
        assert!(response.payload.is_empty());
        assert!(response.seq_nr() == old_response.seq_nr());
        // }

        //fn test_connection_teardown() {
        let old_packet = packet;
        let old_response = response;

        let mut packet = Packet::new();
        packet.set_type(PacketType::Fin);
        packet.set_connection_id(sender_connection_id);
        packet.set_seq_nr(old_packet.seq_nr() + 1);
        packet.set_ack_nr(old_response.seq_nr());

        let response = socket.handle_packet(&packet, client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());

        let response = response.unwrap();

        assert!(response.get_type() == PacketType::State);

        // FIN packets have no payload but the sequence number shouldn't increase
        assert!(packet.seq_nr() == old_packet.seq_nr() + 1);

        // Nor should the ACK packet's sequence number
        assert!(response.seq_nr() == old_response.seq_nr());

        // FIN should be acknowledged
        assert!(response.ack_nr() == packet.seq_nr());

        //}
    }

    #[test]
    fn test_response_to_keepalive_ack() {
        // Boilerplate test setup
        let initial_connection_id: u16 = rand::random();
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());
        let mut socket = iotry!(UtpSocket::bind(server_addr));

        // Establish connection
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(initial_connection_id);

        let response = socket.handle_packet(&packet, client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());
        let response = response.unwrap();
        assert!(response.get_type() == PacketType::State);

        let old_packet = packet;
        let old_response = response;

        // Now, send a keepalive packet
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::State);
        packet.set_connection_id(initial_connection_id);
        packet.set_seq_nr(old_packet.seq_nr() + 1);
        packet.set_ack_nr(old_response.seq_nr());

        let response = socket.handle_packet(&packet, client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_none());

        // Send a second keepalive packet, identical to the previous one
        let response = socket.handle_packet(&packet, client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_none());
    }

    #[test]
    fn test_response_to_wrong_connection_id() {
        // Boilerplate test setup
        let initial_connection_id: u16 = rand::random();
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());
        let mut socket = iotry!(UtpSocket::bind(server_addr));

        // Establish connection
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(initial_connection_id);

        let response = socket.handle_packet(&packet, client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());
        assert!(response.unwrap().get_type() == PacketType::State);

        // Now, disrupt connection with a packet with an incorrect connection id
        let new_connection_id = initial_connection_id.wrapping_mul(2);

        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::State);
        packet.set_connection_id(new_connection_id);

        let response = socket.handle_packet(&packet, client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());

        let response = response.unwrap();
        assert!(response.get_type() == PacketType::Reset);
        assert!(response.ack_nr() == packet.seq_nr());
    }

    #[test]
    fn test_unordered_packets() {
        // Boilerplate test setup
        let initial_connection_id: u16 = rand::random();
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());
        let mut socket = iotry!(UtpSocket::bind(server_addr));

        // Establish connection
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::Syn);
        packet.set_connection_id(initial_connection_id);

        let response = socket.handle_packet(&packet, client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());
        let response = response.unwrap();
        assert!(response.get_type() == PacketType::State);

        let old_packet = packet;
        let old_response = response;

        let mut window: Vec<Packet> = Vec::new();

        // Now, send a keepalive packet
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::Data);
        packet.set_connection_id(initial_connection_id);
        packet.set_seq_nr(old_packet.seq_nr() + 1);
        packet.set_ack_nr(old_response.seq_nr());
        packet.payload = vec!(1,2,3);
        window.push(packet);

        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::Data);
        packet.set_connection_id(initial_connection_id);
        packet.set_seq_nr(old_packet.seq_nr() + 2);
        packet.set_ack_nr(old_response.seq_nr());
        packet.payload = vec!(4,5,6);
        window.push(packet);

        // Send packets in reverse order
        let response = socket.handle_packet(&window[1], client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());
        let response = response.unwrap();
        assert!(response.ack_nr() != window[1].seq_nr());

        let response = socket.handle_packet(&window[0], client_addr);
        assert!(response.is_ok());
        let response = response.unwrap();
        assert!(response.is_some());
    }

    #[test]
    fn test_socket_unordered_packets() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());

        let client = iotry!(UtpSocket::bind(client_addr));
        let mut server = iotry!(UtpSocket::bind(server_addr));

        assert!(server.state == SocketState::New);
        assert!(client.state == SocketState::New);

        // Check proper difference in client's send connection id and receive connection id
        assert_eq!(client.sender_connection_id, client.receiver_connection_id + 1);

        thread::spawn(move || {
            let mut client = iotry!(client.connect(server_addr));
            assert!(client.state == SocketState::Connected);
            let mut s = client.socket;
            let mut window: Vec<Packet> = Vec::new();

            for data in (1..13u8).collect::<Vec<u8>>()[..].chunks(3) {
                let mut packet = Packet::new();
                packet.set_wnd_size(BUF_SIZE as u32);
                packet.set_type(PacketType::Data);
                packet.set_connection_id(client.sender_connection_id);
                packet.set_seq_nr(client.seq_nr);
                packet.set_ack_nr(client.ack_nr);
                packet.payload = data.to_vec();
                window.push(packet.clone());
                client.send_window.push(packet.clone());
                client.seq_nr += 1;
            }

            let mut packet = Packet::new();
            packet.set_wnd_size(BUF_SIZE as u32);
            packet.set_type(PacketType::Fin);
            packet.set_connection_id(client.sender_connection_id);
            packet.set_seq_nr(client.seq_nr);
            packet.set_ack_nr(client.ack_nr);
            window.push(packet);
            client.seq_nr += 1;

            iotry!(s.send_to(&window[3].bytes()[..], server_addr));
            iotry!(s.send_to(&window[2].bytes()[..], server_addr));
            iotry!(s.send_to(&window[1].bytes()[..], server_addr));
            iotry!(s.send_to(&window[0].bytes()[..], server_addr));
            iotry!(s.send_to(&window[4].bytes()[..], server_addr));

            for _ in (0u8..2) {
                let mut buf = [0; BUF_SIZE];
                iotry!(s.recv_from(&mut buf));
            }
        });

        let mut buf = [0u8; BUF_SIZE];
        match server.recv_from(&mut buf) {
            e => println!("{:?}", e),
        }
        // After establishing a new connection, the server's ids are a mirror of the client's.
        assert_eq!(server.receiver_connection_id, server.sender_connection_id + 1);

        assert!(server.state == SocketState::Connected);

        let expected: Vec<u8> = (1..13u8).collect();
        let mut received: Vec<u8> = vec!();
        loop {
            match server.recv_from(&mut buf) {
                Ok((len, _src)) => received.push_all(&buf[..len]),
                Err(ref e) if e.kind == EndOfFile => break,
                Err(e) => panic!("{:?}", e)
            }
        }
        assert_eq!(received.len(), expected.len());
        assert_eq!(received, expected);
    }

    #[test]
    fn test_socket_should_not_buffer_syn_packets() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());
        let server = iotry!(UtpSocket::bind(server_addr));
        let client = iotry!(UdpSocket::bind(client_addr));

        let test_syn_raw = [0x41, 0x00, 0x41, 0xa7, 0x00, 0x00, 0x00,
        0x27, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x3a,
        0xf1, 0x00, 0x00];
        let test_syn_pkt = Packet::decode(&test_syn_raw);
        let seq_nr = test_syn_pkt.seq_nr();

        thread::spawn(move || {
            let mut client = client;
            iotry!(client.send_to(&test_syn_raw, server_addr));
            client.set_timeout(Some(10));
            let mut buf = [0; BUF_SIZE];
            let packet = match client.recv_from(&mut buf) {
                Ok((nread, _src)) => Packet::decode(&buf[..nread]),
                Err(e) => panic!("{}", e),
            };
            assert_eq!(packet.ack_nr(), seq_nr);
            drop(client);
        });

        let mut server = server;
        let mut buf = [0; 20];
        iotry!(server.recv_from(&mut buf));
        assert!(server.ack_nr != 0);
        assert_eq!(server.ack_nr, seq_nr);
        assert!(server.incoming_buffer.is_empty());
    }

    #[test]
    fn test_response_to_triple_ack() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());
        let mut server = iotry!(UtpSocket::bind(server_addr));
        let client = iotry!(UtpSocket::bind(client_addr));

        // Fits in a packet
        const LEN: usize = 1024;
        let data = (0..LEN).map(|idx| idx as u8).collect::<Vec<u8>>();
        let d = data.clone();
        assert_eq!(LEN, data.len());

        thread::spawn(move || {
            let mut client = iotry!(client.connect(server_addr));
            iotry!(client.send_to(&d[..]));
            iotry!(client.close());
        });

        let mut buf = [0; BUF_SIZE];
        // Expect SYN
        iotry!(server.recv_from(&mut buf));

        // Receive data
        let mut data_packet;
        match server.socket.recv_from(&mut buf) {
            Ok((read, _src)) => {
                data_packet = Packet::decode(&buf[..read]);
                assert!(data_packet.get_type() == PacketType::Data);
                assert_eq!(data_packet.payload, data);
                assert_eq!(data_packet.payload.len(), data.len());
            },
            Err(e) => panic!("{}", e),
        }
        let data_packet = data_packet;

        // Send triple ACK
        let mut packet = Packet::new();
        packet.set_wnd_size(BUF_SIZE as u32);
        packet.set_type(PacketType::State);
        packet.set_seq_nr(server.seq_nr);
        packet.set_ack_nr(data_packet.seq_nr() - 1);
        packet.set_connection_id(server.sender_connection_id);

        for _ in (0u8..3) {
            iotry!(server.socket.send_to(&packet.bytes()[..], client_addr));
        }

        // Receive data again and check that it's the same we reported as missing
        match server.socket.recv_from(&mut buf) {
            Ok((0, _)) => panic!("Received 0 bytes from socket"),
            Ok((read, _src)) => {
                let packet = Packet::decode(&buf[..read]);
                assert_eq!(packet.get_type(), PacketType::Data);
                assert_eq!(packet.seq_nr(), data_packet.seq_nr());
                assert!(packet.payload == data_packet.payload);
                let response = server.handle_packet(&packet, client_addr);
                assert!(response.is_ok());
                let response = response.unwrap();
                assert!(response.is_some());
                let response = response.unwrap();
                iotry!(server.socket.send_to(&response.bytes()[..], server.connected_to));
            },
            Err(e) => panic!("{}", e),
        }

        // Receive close
        iotry!(server.recv_from(&mut buf));
    }

    #[test]
    fn test_socket_timeout_request() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());

        let client = iotry!(UtpSocket::bind(client_addr));
        let mut server = iotry!(UtpSocket::bind(server_addr));
        const LEN: usize = 512;
        let data = (0..LEN).map(|idx| idx as u8).collect::<Vec<u8>>();
        let d = data.clone();

        assert!(server.state == SocketState::New);
        assert!(client.state == SocketState::New);

        // Check proper difference in client's send connection id and receive connection id
        assert_eq!(client.sender_connection_id, client.receiver_connection_id + 1);

        thread::spawn(move || {
            let mut client = iotry!(client.connect(server_addr));
            assert!(client.state == SocketState::Connected);
            assert_eq!(client.connected_to, server_addr);
            iotry!(client.send_to(&d[..]));
            drop(client);
        });

        let mut buf = [0u8; BUF_SIZE];
        match server.recv_from(&mut buf) {
            e => println!("{:?}", e),
        }
        // After establishing a new connection, the server's ids are a mirror of the client's.
        assert_eq!(server.receiver_connection_id, server.sender_connection_id + 1);
        assert_eq!(server.connected_to, client_addr);

        assert!(server.state == SocketState::Connected);

        // Purposefully read from UDP socket directly and discard it, in order
        // to behave as if the packet was lost and thus trigger the timeout
        // handling in the *next* call to `UtpSocket.recv_from`.
        iotry!(server.socket.recv_from(&mut buf));

        // Set a much smaller than usual timeout, for quicker test completion
        server.congestion_timeout = 50;

        // Now wait for the previously discarded packet
        loop {
            match server.recv_from(&mut buf) {
                Ok((0, _)) => continue,
                Ok(_) => break,
                Err(e) => panic!("{:?}", e),
            }
        }

        drop(server);
    }

    #[test]
    fn test_sorted_buffer_insertion() {
        let server_addr = next_test_ip4();
        let mut socket = iotry!(UtpSocket::bind(server_addr));

        let mut packet = Packet::new();
        packet.set_seq_nr(1);

        assert!(socket.incoming_buffer.is_empty());

        socket.insert_into_buffer(packet.clone());
        assert_eq!(socket.incoming_buffer.len(), 1);

        packet.set_seq_nr(2);
        packet.set_timestamp_microseconds(128);

        socket.insert_into_buffer(packet.clone());
        assert_eq!(socket.incoming_buffer.len(), 2);
        assert_eq!(socket.incoming_buffer[1].seq_nr(), 2);
        assert_eq!(socket.incoming_buffer[1].timestamp_microseconds(), 128);

        packet.set_seq_nr(3);
        packet.set_timestamp_microseconds(256);

        socket.insert_into_buffer(packet.clone());
        assert_eq!(socket.incoming_buffer.len(), 3);
        assert_eq!(socket.incoming_buffer[2].seq_nr(), 3);
        assert_eq!(socket.incoming_buffer[2].timestamp_microseconds(), 256);

        // Replace a packet with a more recent version
        packet.set_seq_nr(2);
        packet.set_timestamp_microseconds(456);

        socket.insert_into_buffer(packet.clone());
        assert_eq!(socket.incoming_buffer.len(), 3);
        assert_eq!(socket.incoming_buffer[1].seq_nr(), 2);
        assert_eq!(socket.incoming_buffer[1].timestamp_microseconds(), 456);
    }

    #[test]
    fn test_duplicate_packet_handling() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());

        let client = iotry!(UtpSocket::bind(client_addr));
        let mut server = iotry!(UtpSocket::bind(server_addr));

        assert!(server.state == SocketState::New);
        assert!(client.state == SocketState::New);

        // Check proper difference in client's send connection id and receive connection id
        assert_eq!(client.sender_connection_id, client.receiver_connection_id + 1);

        thread::spawn(move || {
            let mut client = iotry!(client.connect(server_addr));
            assert!(client.state == SocketState::Connected);
            let mut s = client.socket.clone();

            let mut packet = Packet::new();
            packet.set_wnd_size(BUF_SIZE as u32);
            packet.set_type(PacketType::Data);
            packet.set_connection_id(client.sender_connection_id);
            packet.set_seq_nr(client.seq_nr);
            packet.set_ack_nr(client.ack_nr);
            packet.payload = vec!(1,2,3);

            // Send two copies of the packet, with different timestamps
            for _ in (0u8..2) {
                packet.set_timestamp_microseconds(now_microseconds());
                iotry!(s.send_to(&packet.bytes()[..], server_addr));
            }
            client.seq_nr += 1;

            // Receive one ACK
            for _ in (0u8..1) {
                let mut buf = [0; BUF_SIZE];
                iotry!(s.recv_from(&mut buf));
            }

            iotry!(client.close());
        });

        let mut buf = [0u8; BUF_SIZE];
        match server.recv_from(&mut buf) {
            e => println!("{:?}", e),
        }
        // After establishing a new connection, the server's ids are a mirror of the client's.
        assert_eq!(server.receiver_connection_id, server.sender_connection_id + 1);

        assert!(server.state == SocketState::Connected);

        let expected: Vec<u8> = vec!(1,2,3);
        let mut received: Vec<u8> = vec!();
        loop {
            match server.recv_from(&mut buf) {
                Ok((len, _src)) => received.push_all(&buf[..len]),
                Err(ref e) if e.kind == EndOfFile => break,
                Err(e) => panic!("{:?}", e)
            }
        }
        assert_eq!(received.len(), expected.len());
        assert_eq!(received, expected);
    }

    #[test]
    fn test_selective_ack_response() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());
        const LEN: usize = 1024 * 10;
        let data = (0..LEN).map(|idx| idx as u8).collect::<Vec<u8>>();
        let to_send = data.clone();

        // Client
        thread::spawn(move || {
            let client = iotry!(UtpSocket::bind(client_addr));
            let mut client = iotry!(client.connect(server_addr));
            client.congestion_timeout = 50;

            iotry!(client.send_to(&to_send[..]));
            iotry!(client.close());
        });

        // Server
        let mut server = iotry!(UtpSocket::bind(server_addr));

        let mut buf = [0; BUF_SIZE];

        // Connect
        iotry!(server.recv_from(&mut buf));

        // Discard packets
        iotry!(server.socket.recv_from(&mut buf));
        iotry!(server.socket.recv_from(&mut buf));
        iotry!(server.socket.recv_from(&mut buf));

        // Generate SACK
        let mut packet = Packet::new();
        packet.set_seq_nr(server.seq_nr);
        packet.set_ack_nr(server.ack_nr - 1);
        packet.set_connection_id(server.sender_connection_id);
        packet.set_timestamp_microseconds(now_microseconds());
        packet.set_type(PacketType::State);
        packet.set_sack(Some(vec!(12, 0, 0, 0)));

        // Send SACK
        iotry!(server.socket.send_to(&packet.bytes()[..], server.connected_to.clone()));

        // Expect to receive "missing" packets
        let mut received: Vec<u8> = vec!();
        loop {
            match server.recv_from(&mut buf) {
                Ok((len, _src)) => received.push_all(&buf[..len]),
                Err(ref e) if e.kind == EndOfFile => break,
                Err(e) => panic!("{:?}", e)
            }
        }
        assert!(!received.is_empty());
        assert_eq!(received.len(), data.len());
        assert_eq!(received, data);
    }

    #[test]
    fn test_correct_packet_loss() {
        let (client_addr, server_addr) = (next_test_ip4(), next_test_ip4());

        let mut server = iotry!(UtpSocket::bind(server_addr));
        let client = iotry!(UtpSocket::bind(client_addr));
        const LEN: usize = 1024 * 10;
        let data = (0..LEN).map(|idx| idx as u8).collect::<Vec<u8>>();
        let to_send = data.clone();

        thread::spawn(move || {
            let mut client = iotry!(client.connect(server_addr));

            // Send everything except the odd chunks
            let chunks = to_send[..].chunks(BUF_SIZE);
            let dst = client.connected_to;
            for (index, chunk) in chunks.enumerate() {
                let mut packet = Packet::new();
                packet.set_seq_nr(client.seq_nr);
                packet.set_ack_nr(client.ack_nr);
                packet.set_connection_id(client.sender_connection_id);
                packet.set_timestamp_microseconds(now_microseconds());
                packet.payload = chunk.to_vec();
                packet.set_type(PacketType::Data);

                if index % 2 == 0 {
                    iotry!(client.socket.send_to(&packet.bytes()[..], dst));
                }

                client.curr_window += packet.len() as u32;
                client.send_window.push(packet);
                client.seq_nr += 1;
            }

            iotry!(client.close());
        });

        let mut buf = [0; BUF_SIZE];
        let mut received: Vec<u8> = vec!();
        loop {
            match server.recv_from(&mut buf) {
                Ok((len, _src)) => received.push_all(&buf[..len]),
                Err(ref e) if e.kind == EndOfFile => break,
                Err(e) => panic!("{}", e)
            }
        }
        assert_eq!(received.len(), data.len());
        assert_eq!(received, data);
    }

    #[test]
    fn test_tolerance_to_small_buffers() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());
        let mut server = iotry!(UtpSocket::bind(server_addr));
        const LEN: usize = 1024;
        let data = (0..LEN).map(|idx| idx as u8).collect::<Vec<u8>>();
        let to_send = data.clone();

        thread::spawn(move || {
            let client = iotry!(UtpSocket::bind(client_addr));
            let mut client = iotry!(client.connect(server_addr));
            iotry!(client.send_to(&to_send[..]));
            iotry!(client.close());
        });

        let mut read = Vec::new();
        while server.state != SocketState::Closed {
            let mut small_buffer = [0; 512];
            match server.recv_from(&mut small_buffer) {
                Ok((0, _src)) => (),
                Ok((len, _src)) => read.push_all(&small_buffer[..len]),
                Err(ref e) if e.kind == EndOfFile => break,
                Err(e) => panic!("{}", e),
            }
        }

        assert_eq!(read.len(), data.len());
        assert_eq!(read, data);
    }

    #[test]
    fn test_sequence_number_rollover() {
        let (server_addr, client_addr) = (next_test_ip4(), next_test_ip4());

        let mut server = iotry!(UtpSocket::bind(server_addr));

        const LEN: usize = BUF_SIZE * 4;
        let data = (0..LEN).map(|idx| idx as u8).collect::<Vec<u8>>();
        let to_send = data.clone();

        thread::spawn(move || {
            let mut client = iotry!(UtpSocket::bind(client_addr));

            // Advance socket's sequence number
            client.seq_nr = ::std::u16::MAX - (to_send.len() / (BUF_SIZE * 2)) as u16;

            let mut client = iotry!(client.connect(server_addr));
            // Send enough data to rollover
            iotry!(client.send_to(&to_send[..]));
            // Check that the sequence number did rollover
            assert!(client.seq_nr < 50);
            // Close connection
            iotry!(client.close());
        });

        let mut buf = [0; BUF_SIZE];
        let mut received: Vec<u8> = vec!();
        loop {
            match server.recv_from(&mut buf) {
                Ok((len, _src)) => received.push_all(&buf[..len]),
                Err(ref e) if e.kind == EndOfFile => break,
                Err(e) => panic!("{}", e)
            }
        }
        assert_eq!(received.len(), data.len());
        assert_eq!(received, data);
    }
}
