//! Native Lounge protocol boundary. This module intentionally owns no actor,
//! task, retry timer, playback policy, or mutable command/session counter.
//! Callers serialize commands and tag owned request snapshots with their epochs.
pub mod events;
pub mod session;
pub mod wire;

pub use events::{Envelope, LoungeEvent};
pub use session::{Command, LoungeError, LoungeStream, LoungeTransport, SessionIds};
pub use wire::{FrameDecoder, FrameError};
