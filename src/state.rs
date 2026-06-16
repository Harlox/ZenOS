use smithay::backend::session::libseat::LibSeatSession;

use crate::backend::Gpu;

/// Shared compositor state, passed as the `&mut data` argument to every calloop
/// event source callback.
pub struct ZenState {
    /// Set to false to break the dispatch loop and exit.
    pub running: bool,
    /// Seat name from the session (usually "seat0").
    pub seat_name: String,
    /// libseat session: grants GPU + input access from a TTY without root.
    pub session: LibSeatSession,
    /// The active GPU (DRM device + renderer + scanout). None until the udev
    /// backend reports the primary GPU via `UdevEvent::Added`.
    ///
    /// Milestone 1-4 assumes a single GPU; multi-GPU / hotplug come later.
    pub gpu: Option<Gpu>,
}

impl ZenState {
    pub fn new(session: LibSeatSession, seat_name: String) -> Self {
        Self {
            running: true,
            seat_name,
            session,
            gpu: None,
        }
    }
}
