//! i3-style scratchpad for rift, built on its window claim API.
//!
//! Rift keybindings drive it with signals:
//!
//! ```toml
//! "Alt + Escape"         = { exec = ["pkill", "-USR1", "-x", "rift-scratchpad"] } # toggle
//! "Alt + Shift + Escape" = { exec = ["pkill", "-USR2", "-x", "rift-scratchpad"] } # park
//! ```
//!
//! Park claims the focused window from rift and moves it offscreen, or hands it back to rift
//! if it is already in the scratchpad. Toggle hides the focused
//! scratchpad window; otherwise it brings back a visible one, or the next parked one, where it
//! last sat, and does nothing with nothing to show. Windows are claimed so that
//! rift suspends focus follows mouse while one of them has focus. Quitting puts parked windows
//! back where they were and hands every claimed window back to rift.
//!
//! Started with `--toggle-parks`, Toggle parks the focused window when there is nothing to show,
//! so one key is enough.

use std::collections::VecDeque;
use std::error::Error;
use std::ptr::{self, NonNull};

use objc2_application_services::{AXError, AXIsProcessTrusted, AXUIElement, AXValue, AXValueType};
use objc2_core_foundation::{
    CFBoolean, CFRetained, CFString, CFType, CGPoint, CGSize, ConcreteType,
};
use rift_client::{
    ClientError, Rect, RiftMachClient, WindowClaimFlags, WindowId, WindowManagementSession,
};

/// Rift hides inactive-workspace windows the same way: all but one pixel past a bottom corner.
const REVEAL_PX: f64 = 1.0;

type AxResult<T> = Result<T, AXError>;

unsafe extern "C-unwind" {
    fn _AXUIElementGetWindow(element: &AXUIElement, id: &mut u32) -> AXError;
}

struct Window {
    id: WindowId,
    ax: CFRetained<AXUIElement>,
    /// Where the window last sat on screen: shown there again, and restored on quit.
    position: CGPoint,
}

struct Scratchpad {
    client: RiftMachClient,
    session: WindowManagementSession,
    hidden: VecDeque<Window>,
    shown: Vec<Window>,
    /// Toggle parks the focused window when there is nothing to show.
    toggle_parks: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    // Blocked before anything else so an early pkill is queued instead of killing us.
    let signals = block_signals(&[
        libc::SIGUSR1,
        libc::SIGUSR2,
        libc::SIGINT,
        libc::SIGTERM,
        libc::SIGHUP,
    ]);
    let toggle_parks = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--toggle-parks") => true,
        Some(other) => {
            return Err(format!("unknown argument {other}, expected --toggle-parks").into());
        }
    };
    if !unsafe { AXIsProcessTrusted() } {
        return Err("needs Accessibility access (System Settings > Privacy & Security)".into());
    }
    let client = RiftMachClient::connect()?;
    let session = client.window_management_session()?;
    let mut pad = Scratchpad {
        client,
        session,
        hidden: VecDeque::new(),
        shown: Vec::new(),
        toggle_parks,
    };
    loop {
        let result = match wait_for(&signals) {
            libc::SIGUSR1 => pad.toggle(),
            libc::SIGUSR2 => pad.park(),
            _ => break,
        };
        if let Err(error) = result {
            eprintln!("rift-scratchpad: {error}");
        }
    }
    pad.restore();
    Ok(())
}

impl Scratchpad {
    fn toggle(&mut self) -> Result<(), Box<dyn Error>> {
        if let Some(window) = focused_window(&self.client)
            .ok()
            .and_then(|(id, _)| self.take_shown(id))
        {
            return self.hide(window);
        }
        let (display, others) = self.displays()?;
        // Rift forgets closed windows; one left queued here would fail every toggle.
        let client = &self.client;
        let open = |window: &Window| {
            !matches!(
                client.get_window_info(window.id),
                Err(ClientError::Server(_))
            )
        };
        self.hidden.retain(open);
        self.shown.retain(open);
        let Some(window) = self.shown.pop().or_else(|| self.hidden.pop_front()) else {
            return if self.toggle_parks {
                self.park()
            } else {
                Ok(())
            };
        };
        if let Err(error) = show(&window, display, &others) {
            // Sent to the back so a window that won't show doesn't block the others.
            self.hidden.push_back(window);
            return Err(ax_error(error));
        }
        self.shown.push(window);
        Ok(())
    }

    fn park(&mut self) -> Result<(), Box<dyn Error>> {
        let (id, ax) = focused_window(&self.client)?;
        if let Some(index) = self.shown.iter().position(|window| window.id == id) {
            self.session.release_window(id)?;
            self.shown.remove(index);
            return Ok(());
        }
        if let Some(index) = self.hidden.iter().position(|window| window.id == id) {
            let window = &self.hidden[index];
            set_position(&window.ax, window.position).map_err(ax_error)?;
            self.session.release_window(id)?;
            self.hidden.remove(index);
            return Ok(());
        }
        let origin = self.client.get_window_info(id)?.frame.origin;
        // Focus follows mouse would otherwise raise whatever the pointer crosses over it.
        self.session
            .claim_window_with_flags(id, WindowClaimFlags::SUPPRESS_FOCUS_FOLLOWS_MOUSE)?;
        let position = CGPoint::new(origin.x, origin.y);
        self.hide(Window { id, ax, position })
    }

    fn hide(&mut self, mut window: Window) -> Result<(), Box<dyn Error>> {
        let moved = self.displays().and_then(|(display, others)| {
            window.position = position(&window.ax).map_err(ax_error)?;
            let size = size(&window.ax).map_err(ax_error)?;
            set_position(&window.ax, hidden_origin(display, &others, size)).map_err(ax_error)
        });
        // Queued even if the move failed, so toggle can still bring it back.
        self.hidden.push_back(window);
        moved
    }

    fn take_shown(&mut self, id: WindowId) -> Option<Window> {
        let index = self.shown.iter().position(|window| window.id == id)?;
        Some(self.shown.remove(index))
    }

    /// The focused display's frame, and the frames of all the others.
    fn displays(&self) -> Result<(Rect, Vec<Rect>), Box<dyn Error>> {
        let (active, others): (Vec<_>, Vec<_>) = self
            .client
            .get_displays()?
            .into_iter()
            .partition(|display| display.is_active_context);
        let display = active
            .first()
            .or(others.first())
            .ok_or("rift reports no displays")?
            .frame;
        Ok((
            display,
            others.iter().map(|display| display.frame).collect(),
        ))
    }

    /// Dropping the session afterwards is what hands the windows back to rift.
    fn restore(&self) {
        for window in &self.hidden {
            let _ = set_position(&window.ax, window.position);
        }
    }
}

/// Shows the window where it last sat, or centred on `display` when that spot is on no display
/// anymore, e.g. after unplugging a monitor.
fn show(window: &Window, display: Rect, others: &[Rect]) -> AxResult<()> {
    let at = window.position;
    let on = |r: &Rect| {
        (r.origin.x..r.origin.x + r.size.width).contains(&at.x)
            && (r.origin.y..r.origin.y + r.size.height).contains(&at.y)
    };
    let position = if on(&display) || others.iter().any(on) {
        at
    } else {
        let size = size(&window.ax)?;
        CGPoint::new(
            display.origin.x + (display.size.width - size.width) / 2.0,
            display.origin.y + (display.size.height - size.height) / 2.0,
        )
    };
    set_position(&window.ax, position)?;
    check(unsafe {
        window
            .ax
            .perform_action(&CFString::from_static_str("AXRaise"))
    })?;
    set(&window.ax, "AXMain", CFBoolean::new(true))?;
    let app = unsafe { AXUIElement::new_application(window.id.pid) };
    set(&app, "AXFrontmost", CFBoolean::new(true))
}

/// All but a pixel past the bottom corner, of `display` or else of another display, that puts
/// the least of the window on any display. macOS keeps the top of a window on screen, so past a
/// corner that borders another display a strip of it shows there.
fn hidden_origin(display: Rect, others: &[Rect], size: CGSize) -> CGPoint {
    let displays: Vec<Rect> = std::iter::once(display)
        .chain(others.iter().copied())
        .collect();
    let overlap = |origin: &CGPoint| {
        displays
            .iter()
            .map(|other| {
                let width = ((origin.x + size.width).min(other.origin.x + other.size.width)
                    - origin.x.max(other.origin.x))
                .max(0.0);
                let height = ((origin.y + size.height).min(other.origin.y + other.size.height)
                    - origin.y.max(other.origin.y))
                .max(0.0);
                width * height
            })
            .sum::<f64>()
    };
    displays
        .iter()
        .flat_map(|corner| {
            let y = corner.origin.y + corner.size.height - REVEAL_PX;
            let right = corner.origin.x + corner.size.width - REVEAL_PX;
            let left = corner.origin.x - size.width + REVEAL_PX;
            [CGPoint::new(right, y), CGPoint::new(left, y)]
        })
        .min_by(|a, b| overlap(a).total_cmp(&overlap(b)))
        .expect("`display` always has corners")
}

fn focused_window(
    client: &RiftMachClient,
) -> Result<(WindowId, CFRetained<AXUIElement>), Box<dyn Error>> {
    // The system-wide AXFocusedApplication query fails with kAXErrorCannotComplete (seen on
    // macOS 27), so the frontmost app comes from rift instead.
    let pid = client
        .get_applications()?
        .into_iter()
        .find(|app| app.is_frontmost)
        .ok_or("rift reports no frontmost app")?
        .pid;
    let app = unsafe { AXUIElement::new_application(pid) };
    let window: CFRetained<AXUIElement> = copy(&app, "AXFocusedWindow").map_err(ax_error)?;
    let mut idx = 0;
    // Rift's window idx is the WindowServer id.
    check(unsafe { _AXUIElementGetWindow(&window, &mut idx) }).map_err(ax_error)?;
    Ok((WindowId { pid, idx }, window))
}

fn size(window: &AXUIElement) -> AxResult<CGSize> {
    read(window, "AXSize", AXValueType::CGSize, CGSize::ZERO)
}

fn position(window: &AXUIElement) -> AxResult<CGPoint> {
    read(window, "AXPosition", AXValueType::CGPoint, CGPoint::ZERO)
}

fn read<T>(
    window: &AXUIElement,
    attribute: &'static str,
    ty: AXValueType,
    mut out: T,
) -> AxResult<T> {
    let value: CFRetained<AXValue> = copy(window, attribute)?;
    let ok = unsafe { value.value(ty, NonNull::from(&mut out).cast()) };
    ok.then_some(out).ok_or(AXError::Failure)
}

fn set_position(window: &AXUIElement, mut point: CGPoint) -> AxResult<()> {
    let value = unsafe { AXValue::new(AXValueType::CGPoint, NonNull::from(&mut point).cast()) }
        .ok_or(AXError::Failure)?;
    set(window, "AXPosition", &value)
}

fn copy<T: ConcreteType>(
    element: &AXUIElement,
    attribute: &'static str,
) -> AxResult<CFRetained<T>> {
    let mut value: *const CFType = ptr::null();
    check(unsafe {
        element.copy_attribute_value(
            &CFString::from_static_str(attribute),
            NonNull::from(&mut value),
        )
    })?;
    let value = NonNull::new(value.cast_mut()).ok_or(AXError::NoValue)?;
    // Copy rule: the caller owns the returned value.
    unsafe { CFRetained::from_raw(value) }
        .downcast::<T>()
        .map_err(|_| AXError::Failure)
}

fn set(element: &AXUIElement, attribute: &'static str, value: &CFType) -> AxResult<()> {
    check(unsafe { element.set_attribute_value(&CFString::from_static_str(attribute), value) })
}

fn check(error: AXError) -> AxResult<()> {
    if error == AXError::Success {
        Ok(())
    } else {
        Err(error)
    }
}

fn ax_error(error: AXError) -> Box<dyn Error> {
    format!("accessibility error {error:?}").into()
}

fn block_signals(signals: &[libc::c_int]) -> libc::sigset_t {
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for &signal in signals {
            libc::sigaddset(&mut set, signal);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, ptr::null_mut());
        set
    }
}

fn wait_for(signals: &libc::sigset_t) -> libc::c_int {
    let mut signal = 0;
    unsafe { libc::sigwait(signals, &mut signal) };
    signal
}

#[cfg(test)]
mod tests {
    use rift_client::{Point, Size};

    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            origin: Point { x, y },
            size: Size { width, height },
        }
    }

    #[test]
    fn hides_past_the_corner_that_avoids_other_displays() {
        let display = rect(0.0, 0.0, 1000.0, 800.0);
        let size = CGSize::new(200.0, 100.0);
        assert_eq!(
            hidden_origin(display, &[], size),
            CGPoint::new(999.0, 799.0)
        );
        let right_neighbour = rect(1000.0, 0.0, 1000.0, 800.0);
        assert_eq!(
            hidden_origin(display, &[right_neighbour], size),
            CGPoint::new(-199.0, 799.0)
        );
        // Between two displays neither of its own corners is free, so it borrows an outer one.
        let left_neighbour = rect(-1000.0, 0.0, 1000.0, 800.0);
        assert_eq!(
            hidden_origin(display, &[left_neighbour, right_neighbour], size),
            CGPoint::new(-1199.0, 799.0)
        );
    }
}
