//! macOS: VTPixelTransferSession scale + BGRA -> NV12 into the encoder's
//! VideoToolbox frames pool, then hevc_videotoolbox.
//!
//! UNTESTED (best effort; no macOS machine was available). It was only
//! type-checked (`cargo check --target aarch64-apple-darwin` against
//! Linux-generated FFmpeg bindings, see crates/cap-encode/README.md); never linked or run.

use super::*;
use std::ffi::c_void;

#[link(name = "VideoToolbox", kind = "framework")]
extern "C" {
    fn VTPixelTransferSessionCreate(allocator: *const c_void, session_out: *mut *mut c_void) -> i32;
    fn VTPixelTransferSessionTransferImage(session: *mut c_void, source: *mut c_void, destination: *mut c_void) -> i32;
    fn VTPixelTransferSessionInvalidate(session: *mut c_void);
    fn VTSessionSetProperty(session: *mut c_void, key: *const c_void, value: *const c_void) -> i32;
    static kVTPixelTransferPropertyKey_DestinationYCbCrMatrix: *const c_void;
}

#[link(name = "CoreVideo", kind = "framework")]
extern "C" {
    static kCVImageBufferYCbCrMatrix_ITU_R_601_4: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: *const c_void);
}

pub struct VtTransfer {
    session: *mut c_void,
}

// SAFETY: VTPixelTransferSession may be used from one thread at a time.
unsafe impl Send for VtTransfer {}

impl VtTransfer {
    pub fn new() -> Result<Self> {
        let mut s: *mut c_void = ptr::null_mut();
        // SAFETY: out-param; framework constants are valid CFStrings.
        unsafe {
            let st = VTPixelTransferSessionCreate(ptr::null(), &mut s);
            if st != 0 || s.is_null() {
                return Err(EncodeError::Ffmpeg(format!("VTPixelTransferSessionCreate: OSStatus {st}")));
            }
            // BT.601 limited, matching the other platforms.
            VTSessionSetProperty(s, kVTPixelTransferPropertyKey_DestinationYCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_601_4);
        }
        Ok(Self { session: s })
    }

    /// Scale/convert the retained capture `CVPixelBufferRef` into a pool frame
    /// (whose `data[3]` is the destination `CVPixelBufferRef`). Half-float
    /// sources are clamped to SDR by the transfer.
    pub fn transfer(&mut self, src: *mut c_void, pool: &BufRef) -> Result<Frame> {
        let frame = Frame::from_hw_pool(pool)?;
        // SAFETY: both are valid CVPixelBufferRefs for the call.
        unsafe {
            let dst = (*frame.as_ptr()).data[3] as *mut c_void;
            let st = VTPixelTransferSessionTransferImage(self.session, src, dst);
            if st != 0 {
                return Err(EncodeError::Ffmpeg(format!("VTPixelTransferSessionTransferImage: OSStatus {st}")));
            }
        }
        Ok(frame)
    }
}

impl Drop for VtTransfer {
    fn drop(&mut self) {
        // SAFETY: we own the session.
        unsafe {
            VTPixelTransferSessionInvalidate(self.session);
            CFRelease(self.session);
        }
    }
}
