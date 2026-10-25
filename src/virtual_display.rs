//! A virtual display, a "second monitor" for the receiver, at exactly its resolution: macOS renders text and
//! windows natively at that size, so nothing gets scaled on the way. Uses CoreGraphics' CGVirtualDisplay
//! classes, which are private (BetterDisplay and DeskPad rely on them too); the display exists while the
//! value lives, and macOS moves its windows back to the main display when it goes.

use std::ffi::{c_char, c_void, CStr};

use anyhow::{bail, Context, Result};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool};
use objc2::msg_send;
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSArray, NSString};

// The largest mode it can switch to later (a rotated or fullscreen view), set once at creation.
const MAX_WIDTH: u32 = 3840;
const MAX_HEIGHT: u32 = 3840;
// macOS offers no Retina mode below about 530 points of height (512 and 520 got 1x; 540 always 2x).
const MIN_RETINA_HEIGHT: u32 = 1080;

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut AnyObject;
}

pub struct VirtualDisplay {
    display: Retained<AnyObject>,
    pub id: u32,
}

impl VirtualDisplay {
    /// A display of `width` x `height` pixels; `hidpi` lays it out at half that in points (Retina), which
    /// suits a close-up screen like an iPad, while a TV wants 1920x1080 at 1x.
    pub fn create(name: &str, width: u32, height: u32, hidpi: bool) -> Result<Self> {
        unsafe {
            let descriptor = new(class(c"CGVirtualDisplayDescriptor")?)?;
            let _: () = msg_send![&*descriptor, setName: &*NSString::from_str(name)];
            // The display reports to this queue (e.g. termination); a private serial one, leaked with it.
            let queue = dispatch_queue_create(c"cast-mirror.display".as_ptr(), std::ptr::null());
            let _: () = msg_send![&*descriptor, setQueue: queue];
            let _: () = msg_send![&*descriptor, setMaxPixelsWide: MAX_WIDTH];
            let _: () = msg_send![&*descriptor, setMaxPixelsHigh: MAX_HEIGHT];
            // About 110 points per inch, like a monitor; macOS derives its default scaling from this.
            let scale = if hidpi { 2.0 } else { 1.0 };
            let size = CGSize::new(width as f64 / scale * 0.23, height as f64 / scale * 0.23);
            let _: () = msg_send![&*descriptor, setSizeInMillimeters: size];
            let _: () = msg_send![&*descriptor, setVendorID: 0x4d43u32];
            let _: () = msg_send![&*descriptor, setProductID: 0x0001u32];
            // macOS refuses a second display with the same serial and remembers the arrangement by it.
            let serial = name.bytes().fold(0x811c_9dc5u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193));
            let _: () = msg_send![&*descriptor, setSerialNum: serial];

            let allocated: *mut AnyObject = msg_send![class(c"CGVirtualDisplay")?, alloc];
            let display: *mut AnyObject = msg_send![allocated, initWithDescriptor: &*descriptor];
            let display = Retained::from_raw(display).context("macOS refused the virtual display")?;
            let id: u32 = msg_send![&*display, displayID];
            let virtual_display = Self { display, id };
            virtual_display.set_mode(width, height, hidpi)?;
            Ok(virtual_display)
        }
    }

    /// Switches to `width` x `height` pixels, keeping the display (and the windows on it).
    pub fn set_mode(&self, width: u32, height: u32, hidpi: bool) -> Result<()> {
        if width > MAX_WIDTH || height > MAX_HEIGHT {
            bail!("{width}x{height} is beyond the virtual display's {MAX_WIDTH}x{MAX_HEIGHT}");
        }
        unsafe {
            let settings = new(class(c"CGVirtualDisplaySettings")?)?;
            let _: () = msg_send![&*settings, setHiDPI: u32::from(hidpi)];
            // In HiDPI the mode is given in points; macOS renders it at twice that in pixels.
            let (w, h) = if hidpi { retina_points(width, height) } else { (width, height) };
            let allocated: *mut AnyObject = msg_send![class(c"CGVirtualDisplayMode")?, alloc];
            let mode: *mut AnyObject = msg_send![allocated, initWithWidth: w, height: h, refreshRate: 60.0f64];
            let mode = Retained::from_raw(mode).context("display mode")?;
            let modes = NSArray::from_retained_slice(&[mode]);
            let _: () = msg_send![&*settings, setModes: &*modes];
            let applied: Bool = msg_send![&*self.display, applySettings: &*settings];
            if !applied.as_bool() {
                bail!("macOS refused the display mode {width}x{height}");
            }
        }
        Ok(())
    }

}

/// `size` times `scale`, in even pixels.
pub fn scaled((width, height): (u32, u32), scale: f64) -> (u32, u32) {
    ((width as f64 * scale) as u32 & !1, (height as f64 * scale) as u32 & !1)
}

/// Half the pixels, but grown to the smallest height macOS still renders at 2x (the capture scales it
/// down to the view again: a few percent, much sharper than 1x scaled up).
fn retina_points(width: u32, height: u32) -> (u32, u32) {
    if height >= MIN_RETINA_HEIGHT {
        return (width / 2, height / 2);
    }
    let w = (width as u64 * MIN_RETINA_HEIGHT as u64 / height as u64) as u32;
    (w / 2, MIN_RETINA_HEIGHT / 2)
}

fn class(name: &CStr) -> Result<&'static AnyClass> {
    AnyClass::get(name).with_context(|| format!("{name:?} is missing: macOS changed its private display API"))
}

unsafe fn new(class: &AnyClass) -> Result<Retained<AnyObject>> {
    let object: *mut AnyObject = unsafe { msg_send![class, new] };
    unsafe { Retained::from_raw(object) }.context("object creation failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retina_points_grow_to_the_lowest_retina_height() {
        assert_eq!(retina_points(2388, 1668), (1194, 834));
        assert_eq!(retina_points(2622, 1014), (1396, 540));
        assert_eq!(scaled((2388, 1668), 1.25), (2984, 2084));
    }

    #[test]
    #[ignore = "creates a display on this Mac for a moment"]
    fn creates_a_hidpi_display_of_the_asked_size() {
        // An iPhone's landscape view: too low for Retina as it is.
        let display = VirtualDisplay::create("Cast Mirror test", 2622, 1014, true).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        let mode = objc2_core_graphics::CGDisplayCopyDisplayMode(display.id).unwrap();
        assert_eq!(objc2_core_graphics::CGDisplayMode::pixel_height(Some(&mode)), 1080);
        assert_eq!(objc2_core_graphics::CGDisplayMode::height(Some(&mode)), 540);
        display.set_mode(1668, 2388, true).unwrap();
    }
}
