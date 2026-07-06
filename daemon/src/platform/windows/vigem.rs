//! Virtual Xbox 360 gamepads via ViGEmBus, mirroring the semantics of
//! platform/linux/uinput.rs's `VirtualGamepad` (up to 4 pads keyed by
//! controller id, last-write-wins state, drop-to-unplug).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

use crate::input::{
    GamepadState, GPAD_BTN_EAST, GPAD_BTN_MODE, GPAD_BTN_NORTH, GPAD_BTN_SELECT, GPAD_BTN_SOUTH,
    GPAD_BTN_START, GPAD_BTN_THUMBL, GPAD_BTN_THUMBR, GPAD_BTN_TL, GPAD_BTN_TR, GPAD_BTN_WEST,
};

/// A pad's target plus whether its current run of update() failures has
/// already been logged (reset on re-attach).
struct Pad {
    target: Xbox360Wired<Arc<Client>>,
    err_logged: bool,
}

/// Manages a lazily-connected ViGEmBus client and up to 4 Xbox 360 wired
/// targets keyed by controller id.
pub struct VirtualGamepads {
    client: Option<Arc<Client>>,
    pads: HashMap<u8, Pad>,
}

impl VirtualGamepads {
    pub fn new() -> Self {
        Self {
            client: None,
            pads: HashMap::new(),
        }
    }

    /// Connects lazily and caches the client; a failed connect leaves
    /// `self.client` unset so the next attach attempt retries instead of
    /// being poisoned by this one.
    fn client(&mut self) -> Result<Arc<Client>> {
        if let Some(ref client) = self.client {
            return Ok(Arc::clone(client));
        }
        let client = Arc::new(
            Client::connect()
                .context("ViGEmBus driver not installed — gamepad passthrough unavailable")?,
        );
        self.client = Some(Arc::clone(&client));
        Ok(client)
    }

    pub fn attach(&mut self, id: u8) -> Result<()> {
        if self.pads.contains_key(&id) {
            return Ok(());
        }
        let client = self.client()?;
        let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
        target
            .plugin()
            .context("failed to plug in virtual gamepad")?;
        // wait_ready() blocks on a driver IOCTL that some ViGEmBus versions/
        // states never complete; update() already reports TargetNotReady per
        // call, so don't unplug (drop) a successfully plugged target just
        // because this wait failed.
        if let Err(e) = target.wait_ready() {
            eprintln!(
                "[gamepad] virtual gamepad {} readiness wait failed ({e}); continuing anyway",
                id + 1
            );
        }
        self.pads.insert(
            id,
            Pad {
                target,
                err_logged: false,
            },
        );
        println!("[input] virtual gamepad {} connected (ViGEm)", id + 1);
        Ok(())
    }

    pub fn detach(&mut self, id: u8) {
        if self.pads.remove(&id).is_some() {
            println!("[input] virtual gamepad {} disconnected (ViGEm)", id + 1);
        }
    }

    pub fn state(&mut self, id: u8, st: &GamepadState) -> Result<()> {
        let Some(pad) = self.pads.get_mut(&id) else {
            // Matches Linux: a state update for an unattached slot is ignored.
            return Ok(());
        };
        if let Err(e) = pad.target.update(&to_xgamepad(st)) {
            // The caller discards this Err, so log the first failure per pad
            // ourselves and stay quiet until it's re-attached.
            if !pad.err_logged {
                eprintln!("[gamepad] virtual gamepad {} update failed: {e}", id + 1);
                pad.err_logged = true;
            }
            return Err(e).context("failed to update virtual gamepad state");
        }
        Ok(())
    }
}

fn to_xgamepad(st: &GamepadState) -> XGamepad {
    let mut buttons = 0u16;
    let mut map = |bit: u16, xbit: u16| {
        if st.buttons & bit != 0 {
            buttons |= xbit;
        }
    };
    map(GPAD_BTN_SOUTH, XButtons::A);
    map(GPAD_BTN_EAST, XButtons::B);
    map(GPAD_BTN_WEST, XButtons::X);
    map(GPAD_BTN_NORTH, XButtons::Y);
    map(GPAD_BTN_TL, XButtons::LB);
    map(GPAD_BTN_TR, XButtons::RB);
    map(GPAD_BTN_THUMBL, XButtons::LTHUMB);
    map(GPAD_BTN_THUMBR, XButtons::RTHUMB);
    map(GPAD_BTN_SELECT, XButtons::BACK);
    map(GPAD_BTN_START, XButtons::START);
    map(GPAD_BTN_MODE, XButtons::GUIDE);

    // hat_x/hat_y follow evdev ABS_HAT0X/Y convention (-1/0/1); dpad has no
    // separate button bits on the wire.
    if st.hat_x < 0 {
        buttons |= XButtons::LEFT;
    } else if st.hat_x > 0 {
        buttons |= XButtons::RIGHT;
    }
    if st.hat_y < 0 {
        buttons |= XButtons::UP;
    } else if st.hat_y > 0 {
        buttons |= XButtons::DOWN;
    }

    XGamepad {
        buttons: XButtons(buttons),
        // Wire triggers are 0-1023 (evdev ABS_Z/RZ range); XInput wants u8.
        left_trigger: (st.lt >> 2) as u8,
        right_trigger: (st.rt >> 2) as u8,
        thumb_lx: st.lx,
        // evdev ABS_Y/RY are down-positive; XInput thumb Y is up-positive.
        thumb_ly: st.ly.saturating_neg(),
        thumb_rx: st.rx,
        thumb_ry: st.ry.saturating_neg(),
    }
}
