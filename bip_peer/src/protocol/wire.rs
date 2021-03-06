use std::sync::mpsc::{self, Receiver};
use std::error::Error;
use std::collections::{VecDeque, HashMap};
use std::collections::hash_map::Entry;
use std::time::Duration;
use std::marker::PhantomData;
use std::any::Any;

use bip_handshake::{BTContext, BTSeed};
use bip_handshake::protocol::{PeerProtocol, LocalAddress, TryBind, TryAccept, TryConnect};
use bip_util::bt::{PeerId, InfoHash};
use bip_util::send::{TrySender, SplitSender};
use rotor::{Scope, Time};
use rotor::mio::Evented;
use rotor::mio::tcp::TcpStream;
use rotor_stream::{Protocol, Intent, Exception, Transport, Buf, StreamSocket, SocketError};
use nom::IResult;

use disk::{DiskManager, IDiskMessage, ODiskMessage, DiskManagerAccess};
use message::{self, MessageType};
use protocol::{PeerIdentifier, IProtocolMessage, ProtocolSender, OProtocolMessage, OProtocolMessageKind};
use protocol::context::WireContext;
use protocol::error::{ProtocolError, ProtocolErrorKind};
use selector::{OSelectorMessage, OSelectorMessageKind};
use token::Token;

// Max messages incoming to our connection from both the selection thread and disk thread.
pub const MAX_INCOMING_MESSAGES: usize = 8;

// Since we check the peer timeout lazily (because we can't have more than one timer going
// without reimplementing a timer wheel ourselves...) in the worst case we can assume a
// peer hasn't sent us a message for 1:59 (right before a timeout) + 1:30 (our own timeout,
// or, worst case time until the peer timeout is checked again) or 3 minutes and 29 seconds.
const MAX_PEER_TIMEOUT_MILLIS: u64 = 2 * 60 * 1000;
const MAX_SELF_TIMEOUT_MILLIS: u64 = (30 + 60) * 1000;

/// Implementation of the peer wire protocol.
pub struct WireProtocol<L, DR> {
    id: PeerIdentifier,
    hash: InfoHash,
    disk: DR,
    send: SplitSender<ProtocolSender>,
    recv: Receiver<IProtocolMessage>,
    state: WireState,
    // Any writes that can immediately be executed are
    // placed inside of this queue, during a state transition
    // this queue will be checked and popped from.
    write_queue: VecDeque<(MessageType, Option<Token>)>,
    // Any writes that require the use of a block of data will
    // immediately be placed here after contacting the disk manager.
    // When the disk manager responds, the message will be taken
    // out of this queue and placed at the end of the write queue.
    block_queue: HashMap<Token, MessageType>,
    last_sent: Time,
    last_recvd: Time,
    _listener: PhantomData<L>,
}

/// Enumeration for all states that a peer can be in in terms of messages being sent or received.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum WireState {
    /// Read the message length; default state.
    ///
    /// Valid to transition from this state to either ReadPayload or WritePayload.
    ReadLength,
    /// Read the message length + the message itself.
    ReadPayload(usize),
    /// Wait for the disk to reserve memory for the block.
    DiskReserve(Token, usize),
    /// Write (flush) a single message to the peer.
    WritePayload,
}

impl<L, DR> WireProtocol<L, DR>
    where DR: TrySender<IDiskMessage> + DiskManagerAccess {
    /// Create a new WireConnection and return an Intent.
    fn new(id: PeerIdentifier,
           hash: InfoHash,
           disk: DR,
           send: SplitSender<ProtocolSender>,
           recv: Receiver<IProtocolMessage>,
           now: Time)
           -> Intent<WireProtocol<L, DR>> {
        let connection = WireProtocol {
            id: id,
            hash: hash,
            state: WireState::ReadLength,
            disk: disk,
            send: send,
            recv: recv,
            write_queue: VecDeque::new(),
            block_queue: HashMap::new(),
            last_sent: now,
            last_recvd: now,
            _listener: PhantomData,
        };

        let self_timeout = connection.self_timeout(now);
        Intent::of(connection).expect_bytes(message::MESSAGE_LENGTH_LEN_BYTES).deadline(self_timeout)
    }

    /// Returns true if the peer has exceeded it's timeout (no message received for a while).
    fn peer_timeout(&self, now: Time) -> bool {
        let max_peer_timeout = Duration::from_millis(MAX_PEER_TIMEOUT_MILLIS);

        // Since Time does not implement Sub, we convert (now - recvd > timeout) to (now > recvd + timeout)
        now > self.last_recvd + max_peer_timeout
    }

    /// Returns the timeout for ourselves at which point we will send a keep alive message.
    fn self_timeout(&self, now: Time) -> Time {
        now + Duration::from_millis(MAX_SELF_TIMEOUT_MILLIS)
    }

    /// Send the message to the disk manager.
    fn send_disk_message(&self, msg: IDiskMessage) {
        if self.disk.try_send(msg).is_some() {
            panic!("bip_peer: Wire Protocol Failed To Send Message To Disk Manager")
        }
    }

    /// Process the message to be written to the remote peer.
    ///
    /// Returns true if a disconnnect from the peer should be initiated.
    fn process_message(&mut self, now: Time, msg: OSelectorMessage) -> bool {
        // Check for any bugs in the selection layer sending us an invalid peer identifier
        if msg.id() != self.id {
            panic!("bip_peer: Protocol Layer Received Invalid Message ID From Selection Layer, Received: {:?} Expected: {:?}",
                   msg.id(),
                   self.id);
        }
        self.last_sent = now;

        match msg.kind() {
            OSelectorMessageKind::PeerKeepAlive => self.write_queue.push_back((MessageType::KeepAlive, None)),
            OSelectorMessageKind::PeerDisconnect => (),
            OSelectorMessageKind::PeerChoke => self.write_queue.push_back((MessageType::Choke, None)),
            OSelectorMessageKind::PeerUnChoke => self.write_queue.push_back((MessageType::UnChoke, None)),
            OSelectorMessageKind::PeerInterested => self.write_queue.push_back((MessageType::Interested, None)),
            OSelectorMessageKind::PeerNotInterested => self.write_queue.push_back((MessageType::UnInterested, None)),
            OSelectorMessageKind::PeerHave(have_msg) => self.write_queue.push_back((MessageType::Have(have_msg), None)),
            OSelectorMessageKind::PeerBitField(bfield_msg) => self.write_queue.push_back((MessageType::BitField(bfield_msg), None)),
            OSelectorMessageKind::PeerRequest(req_msg) => self.write_queue.push_back((MessageType::Request(req_msg), None)),
            OSelectorMessageKind::PeerPiece(piece_msg) => {
                let token = self.disk.new_request_token();

                // Tell the disk manager to load the piece that we need to send, then store the token to lookup when we get a response
                self.send_disk_message(IDiskMessage::LoadBlock(token, self.hash, piece_msg));
                self.block_queue.insert(token, MessageType::Piece(piece_msg));
            }
            OSelectorMessageKind::PeerCancel(cancel_msg) => self.write_queue.push_back((MessageType::Cancel(cancel_msg), None)),
        }

        msg.kind() == OSelectorMessageKind::PeerDisconnect
    }

    /// Process the disk event for the given token which may or may not advance our state.
    fn process_disk(&mut self, in_buffer: &mut Buf, token: Token) {
        let curr_state = self.state;

        let opt_message_type = self.block_queue.remove(&token);
        match (opt_message_type, curr_state) {
            (Some(message_type), _) => {
                // Disk manager has loaded a block for us to write to the peer, move the message to our write_queue
                self.write_queue.push_back((message_type, Some(token)));
            }
            (None, WireState::DiskReserve(tok, len)) if tok == token => {
                // Disk manager has reserved a block for us to write our received block to
                self.disk.write_block(token, &in_buffer[..len]);
                self.send_disk_message(IDiskMessage::ProcessBlock(token));

                in_buffer.consume(len);
                self.state = WireState::ReadLength;
            }
            (None, WireState::DiskReserve(tok, len)) => unreachable!("bip_peer: Token Returned By DiskManager Was Not Expected"),
            _ => unreachable!("bip_peer: Called ProcessDisk In An Invalid State {:?}", curr_state),
        };
    }

    /// Transition our state into a disconnected state.
    fn advance_disconnect<F>(self, sel_send: F, error: ProtocolError) -> Intent<WireProtocol<L, DR>>
        where F: Fn(OProtocolMessage)
    {
        sel_send(OProtocolMessage::new(self.id, OProtocolMessageKind::PeerDisconnect));

        Intent::error(Box::new(error))
    }

    /// Attempts to advance our state from a read event.
    fn advance_read<F>(mut self, now: Time, in_buffer: &mut Buf, out_buffer: &mut Buf, sel_send: F) -> Intent<WireProtocol<L, DR>>
        where F: Fn(OProtocolMessage)
    {
        let curr_state = self.state;

        match curr_state {
            WireState::ReadLength => {
                // Don't consume the bytes that make up the length, add that back into the expected length
                let expected_len = message::parse_message_length(&in_buffer[..]) + message::MESSAGE_LENGTH_LEN_BYTES;
                self.state = WireState::ReadPayload(expected_len);
            }
            WireState::ReadPayload(len) => {
                let res_opt_kind_msg = parse_kind_message(self.id, &in_buffer[..len], self.disk.new_request_token());

                // For whatever message we received, propogate it up a layer (it is impossible to
                // receive a peer disconnect message off the wire, so we assume we arent propogating
                // that message)
                match res_opt_kind_msg {
                    Ok(Some(OProtocolMessageKind::PeerPiece(token, piece_msg))) => {
                        in_buffer.consume(len - piece_msg.block_length());
                        self.state = WireState::DiskReserve(token, piece_msg.block_length());

                        // Disk manager will notify us when the memory is reserved
                        self.send_disk_message(IDiskMessage::ReserveBlock(token, self.hash, piece_msg));
                        sel_send(OProtocolMessage::new(self.id, OProtocolMessageKind::PeerPiece(token, piece_msg)));
                    }
                    Ok(opt_kind) => {
                        in_buffer.consume(len);
                        self.state = WireState::ReadLength;

                        if let Some(kind) = opt_kind {
                            sel_send(OProtocolMessage::new(self.id, kind));
                        }
                    }
                    Err(prot_error) => {
                        // Early return, peer gave us an invalid message
                        return self.advance_disconnect(sel_send, prot_error);
                    }
                }
            }
            _ => unreachable!("bip_peer: Called AdvanceRead In An Invalid State {:?}", curr_state),
        }

        self.advance_write(now, out_buffer, false)
    }

    /// Attempts to advance our state to/from a write event.
    ///
    /// Since we are working with a half duplex abstraction, anytime we transition from some state back to WireState::ReadLength,
    /// we should attempt to transition into a write state (we aggressively try to transition to a write) because that is the only
    /// time we can take control of the stream and write to the peer. The upper layer will have to make sure that it doesn't starve
    /// ourselves of reads (since there is no notion of "flow" control provided back to that layer). We may want to look at this again
    /// in the future and provide the upper layer with feedback for when writes succeeded, although it would be preferrable to not do so.
    fn advance_write(mut self, now: Time, mut out_buffer: &mut Buf, bytes_flushed: bool) -> Intent<WireProtocol<L, DR>> {
        // First, check if this was called from a bytes flushed event
        if bytes_flushed {
            // "Reset" our state
            self.state = WireState::ReadLength;

            // Ack the write
            self.send.sender_ack().ack();
        }

        // Next, check if we can transition to/back to a write event
        if !self.write_queue.is_empty() && self.state == WireState::ReadLength {
            let (msg, opt_token) = self.write_queue.pop_front().unwrap();

            // We can write out this message, and an optional payload from disk
            msg.write_bytes(&mut out_buffer).unwrap();
            if let Some(token) = opt_token {
                self.disk.read_block(token, out_buffer);
                self.send_disk_message(IDiskMessage::ReclaimBlock(token));
            }

            self.state = WireState::WritePayload;
        }

        // Figure our what intent we should return based on our CURRENT state, even if unchanged
        let self_timeout = self.self_timeout(now);
        match self.state {
            WireState::ReadLength => Intent::of(self).expect_bytes(message::MESSAGE_LENGTH_LEN_BYTES).deadline(self_timeout),
            WireState::ReadPayload(len) => Intent::of(self).expect_bytes(len).deadline(self_timeout),
            WireState::DiskReserve(..) => Intent::of(self).sleep().deadline(self_timeout),
            WireState::WritePayload => Intent::of(self).expect_flush().deadline(self_timeout),
        }
    }
}

/// Attempt to parse the peer message as an OProtocolMessageKind.
fn parse_kind_message(id: PeerIdentifier, bytes: &[u8], request_token: Token) -> Result<Option<OProtocolMessageKind>, ProtocolError> {
    match MessageType::from_bytes(bytes) {
        IResult::Done(_, msg_type) => Ok(map_message_type(msg_type, request_token)),
        IResult::Error(_) |
        IResult::Incomplete(_) => Err(ProtocolError::new(id, ProtocolErrorKind::InvalidMessage)),
    }
}

/// Maps a message type as an OProtocolMessageKind.
fn map_message_type(msg_type: MessageType, request_token: Token) -> Option<OProtocolMessageKind> {
    match msg_type {
        MessageType::KeepAlive => None,
        MessageType::Choke => Some(OProtocolMessageKind::PeerChoke),
        MessageType::UnChoke => Some(OProtocolMessageKind::PeerUnChoke),
        MessageType::Interested => Some(OProtocolMessageKind::PeerInterested),
        MessageType::UnInterested => Some(OProtocolMessageKind::PeerUnInterested),
        MessageType::Have(msg) => Some(OProtocolMessageKind::PeerHave(msg)),
        MessageType::BitField(msg) => Some(OProtocolMessageKind::PeerBitField(msg)),
        MessageType::Request(msg) => Some(OProtocolMessageKind::PeerRequest(msg)),
        MessageType::Piece(msg) => Some(OProtocolMessageKind::PeerPiece(request_token, msg)),
        MessageType::Cancel(msg) => Some(OProtocolMessageKind::PeerCancel(msg)),
        MessageType::Extension(_) => unimplemented!(),
    }
}

impl<L, DR> PeerProtocol for WireProtocol<L, DR>
    where L: LocalAddress + TryBind + TryAccept + Evented + Any + Send,
          L::Output: TryConnect + StreamSocket + Send,
          DR: DiskManagerAccess + TrySender<IDiskMessage>
{
    type Context = WireContext<DR>;

    type Protocol = Self;

    type Listener = L;

    type Socket = L::Output;
}

impl<L, DR> Protocol for WireProtocol<L, DR>
    where L: LocalAddress + TryBind + TryAccept + Evented + Any + Send,
          L::Output: TryConnect + StreamSocket + Send,
          DR: DiskManagerAccess + TrySender<IDiskMessage>
{
    type Context = BTContext<WireContext<DR>>;
    type Socket = L::Output;
    type Seed = BTSeed;

    fn create(bt_seed: Self::Seed, sock: &mut Self::Socket, scope: &mut Scope<Self::Context>) -> Intent<Self> {
        let id = PeerIdentifier::new(bt_seed.addr(), bt_seed.pid());

        // Create a ProtocolSender for layers to send messages and wake us up
        let (send, recv) = mpsc::sync_channel(MAX_INCOMING_MESSAGES + 1);
        let protocol_send = ProtocolSender::new(send, scope.notifier());

        // Using a SplitSender for the sender here so that we can defer message acking until the message is queued and written
        let select_send = SplitSender::new(protocol_send.clone(), MAX_INCOMING_MESSAGES);
        scope.send_selector(OProtocolMessage::new(id, OProtocolMessageKind::PeerConnect(Box::new(select_send.clone()), bt_seed.hash())));

        let active_disk = scope.register_disk(Box::new(protocol_send));

        WireProtocol::new(id, bt_seed.hash(), active_disk, select_send, recv, scope.now())
    }

    fn bytes_read(self, transport: &mut Transport<Self::Socket>, end: usize, scope: &mut Scope<Self::Context>) -> Intent<Self> {
        let now = scope.now();
        let id = self.id;

        if self.peer_timeout(now) {
            self.advance_disconnect(|msg| scope.send_selector(msg), ProtocolError::new(id, ProtocolErrorKind::RemoteTimeout))
        } else {
            let (input, output) = transport.buffers();

            self.advance_read(now, input, output, |msg| scope.send_selector(msg))
        }
    }

    fn bytes_flushed(self, transport: &mut Transport<Self::Socket>, scope: &mut Scope<Self::Context>) -> Intent<Self> {
        let now = scope.now();
        let id = self.id;

        if self.peer_timeout(now) {
            self.advance_disconnect(|msg| scope.send_selector(msg), ProtocolError::new(id, ProtocolErrorKind::RemoteTimeout))
        } else {
            self.advance_write(now, transport.output(), true)
        }
    }

    fn timeout(mut self, transport: &mut Transport<Self::Socket>, scope: &mut Scope<Self::Context>) -> Intent<Self> {
        let now = scope.now();
        let id = self.id;

        if self.peer_timeout(now) {
            self.advance_disconnect(|msg| scope.send_selector(msg), ProtocolError::new(id, ProtocolErrorKind::RemoteTimeout))
        } else {
            // All we can do here is push a keep alive message on to our queue since we can't necessarily transition to a write payload state
            // for example, if we are still waiting on the disk manager. Also, we will update our message_sent whenever we push to the write
            // queue to make it easy for us to know what we mean when we talk about our write timeout.
            let id = self.id;
            // Don't care if it didnt go through, that means there are pending writes
            self.send.try_send(OSelectorMessage::new(id, OSelectorMessageKind::PeerKeepAlive));

            self.advance_write(now, transport.output(), false)
        }
    }

    fn exception(self, _transport: &mut Transport<Self::Socket>, reason: Exception, _scope: &mut Scope<Self::Context>) -> Intent<Self> {
        let id = self.id;

        self.advance_disconnect(|msg| _scope.send_selector(msg), ProtocolError::new(id, ProtocolErrorKind::RemoteDisconnect))
    }

    fn fatal(self, reason: Exception, scope: &mut Scope<Self::Context>) -> Option<Box<Error>> {
        let id = self.id;
        let _ = self.advance_disconnect(|msg| scope.send_selector(msg), ProtocolError::new(id, ProtocolErrorKind::RemoteError));

        None
    }

    fn wakeup(mut self, transport: &mut Transport<Self::Socket>, scope: &mut Scope<Self::Context>) -> Intent<Self> {
        let now = scope.now();
        let id = self.id;

        if self.peer_timeout(now) {
            self.advance_disconnect(|msg| scope.send_selector(msg), ProtocolError::new(id, ProtocolErrorKind::RemoteTimeout))
        } else {
            while let Ok(msg) = self.recv.try_recv() {
                match msg {
                    // We don't use the namespace here because we know it is the same (TODO, Should Pass Namespace)
                    IProtocolMessage::DiskManager(ODiskMessage::BlockLoaded(_namespace, token)) | 
                    IProtocolMessage::DiskManager(ODiskMessage::BlockReserved(_namespace, token)) => {
                        self.process_disk(transport.input(), token);
                    },
                    IProtocolMessage::DiskManager(_) => {
                        panic!("bip_peer: WireProtocol Received Unexpected Message From DiskManager")
                    },
                    IProtocolMessage::PieceManager(sel_msg) => {
                        // If the selection layer sent us a disconnect message, handle it here
                        if self.process_message(now, sel_msg) {
                            // Since the selection layer initiated the disconnect, dont send the disconnect to them
                            return self.advance_disconnect(|_| (), ProtocolError::new(id, ProtocolErrorKind::RemoteDisconnect));
                        }
                    }
                }
            }

            self.advance_write(now, transport.output(), false)
        }
    }
}
