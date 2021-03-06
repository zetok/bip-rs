use std::time::Duration;
use std::default::Default;

const DEFAULT_HANDSHAKE_BUFFER_SIZE: usize = 1000;
const DEFAULT_WAIT_BUFFER_SIZE:      usize = 10;
const DEFAULT_DONE_BUFFER_SIZE:      usize = 10;

/// Once we get parallel handshake support (requires
/// mpmc future channel support, we can bump this up).
const DEFAULT_HANDSHAKE_TIMEOUT_MILLIS: u64 = 1000;

/// Configures the internals of a `Handshaker`.
#[derive(PartialEq, Eq, Debug, Copy, Clone)]
pub struct HandshakerConfig {
    sink_buffer_size:  usize,
    wait_buffer_size:  usize,
    done_buffer_size:  usize,
    handshake_timeout: Duration
}

impl HandshakerConfig {
    /// Sets the buffer size that the `HandshakeSink` uses internally
    /// to hold `InitiateMessage`s before they are processed.
    pub fn set_sink_buffer_size(&mut self, size: usize) {
        self.sink_buffer_size = size;
    }
    
    /// Gets the sink buffer size.
    pub fn sink_buffer_size(&self) -> usize {
        self.sink_buffer_size
    }

    /// Sets the buffer size that `Handshaker` uses internally
    /// to store handshake connections before they are processed.
    pub fn set_wait_buffer_size(&mut self, size: usize) {
        self.wait_buffer_size = size;
    }

    /// Gets the wait buffer size.
    pub fn wait_buffer_size(&self) -> usize {
        self.wait_buffer_size
    }

    /// Sets the buffer size that `HandshakeStream` uses internally
    /// to store processed handshake connections before they are yielded.
    pub fn set_done_buffer_size(&mut self, size: usize) {
        self.done_buffer_size = size;
    }

    /// Gets the done buffer size.
    pub fn done_buffer_size(&self) -> usize {
        self.done_buffer_size
    }

    /// Sets the handshake timeout that `Handshaker` uses to
    /// make sure peers dont take too long to respond to us.
    pub fn set_handshake_timeout(&mut self, timeout: Duration) {
        self.handshake_timeout = timeout;
    }

    /// Gets the handshake timeout.
    pub fn handshake_timeout(&self) -> Duration {
        self.handshake_timeout
    }
}

impl Default for HandshakerConfig {
    fn default() -> HandshakerConfig {
        HandshakerConfig {
            sink_buffer_size: DEFAULT_HANDSHAKE_BUFFER_SIZE,
            wait_buffer_size: DEFAULT_WAIT_BUFFER_SIZE,
            done_buffer_size: DEFAULT_DONE_BUFFER_SIZE,
            handshake_timeout: Duration::from_millis(DEFAULT_HANDSHAKE_TIMEOUT_MILLIS)
         }
    }
}