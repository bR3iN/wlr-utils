//! EGL/GLES core: dma-buf → GL texture import and headless readback.
//!
//! This is the windowing- and egui-free half of the GPU path. It loads the
//! `EGL_EXT_image_dma_buf_import` entry points and wraps a capture dma-buf as an
//! `EGLImage`, and provides [`GpuReadback`] — an offscreen context that reads such a
//! dma-buf back to CPU RGBA8. The egui display toolkit ([`crate::render`], gated by
//! the `toolkit` feature) builds on these primitives; this module stays available in
//! headless builds (record/timelapse) that need readback but no UI.
//!
//! Extension function pointers are loaded at runtime via `eglGetProcAddress`
//! (edgefirst-egl has no typed bindings for these).

use crate::error::{CaptureError, Context, Result};
use crate::wl;
use edgefirst_egl as egl;
use std::ffi::c_void;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use wayland_client::Connection;

pub(crate) type Egl = egl::Instance<egl::Dynamic<libloading::Library, egl::EGL1_4>>;

pub(crate) type EglImage = *mut c_void;
pub(crate) const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_ATTRIB_NONE: i32 = 0x3038;

/// Per-plane attribute names, in plane order: fd, offset, pitch, modifier lo/hi.
/// The enum values are not contiguous across planes (planes 0–2 come from
/// `EGL_EXT_image_dma_buf_import`, plane 3 was added later), so they are tabulated
/// rather than computed.
const PLANE_ATTRS: [[i32; 5]; 4] = [
    [0x3272, 0x3273, 0x3274, 0x3443, 0x3444],
    [0x3275, 0x3276, 0x3277, 0x3445, 0x3446],
    [0x3278, 0x3279, 0x327A, 0x3447, 0x3448],
    [0x3440, 0x3441, 0x3442, 0x3449, 0x344A],
];
pub(crate) const GL_TEXTURE_2D: u32 = 0x0DE1;

type EglCreateImageKhr =
    unsafe extern "system" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> EglImage;
type EglDestroyImageKhr = unsafe extern "system" fn(*mut c_void, EglImage) -> u32;
type GlEglImageTargetTexture2dOes = unsafe extern "system" fn(u32, EglImage);

/// Resolved EGL/GL extension entry points + the EGL display, for dma-buf import.
#[derive(Clone, Copy)]
pub(crate) struct DmabufEgl {
    pub(crate) display: *mut c_void,
    pub(crate) create_image: EglCreateImageKhr,
    pub(crate) destroy_image: EglDestroyImageKhr,
    pub(crate) image_target: GlEglImageTargetTexture2dOes,
    /// Whether `EGL_EXT_image_dma_buf_import_modifiers` is available. Without it
    /// the modifier attributes are not accepted, and only implicit-layout,
    /// single-plane buffers can be imported.
    pub(crate) modifiers: bool,
}

/// Load the dma-buf import entry points. `None` if the driver lacks them (then there
/// is no GPU import path and callers fall back to whatever shm provided).
pub(crate) fn load_dmabuf_egl(egl: &Egl, display: egl::Display) -> Option<DmabufEgl> {
    let create = egl.get_proc_address("eglCreateImageKHR")?;
    let destroy = egl.get_proc_address("eglDestroyImageKHR")?;
    let target = egl.get_proc_address("glEGLImageTargetTexture2DOES")?;
    let modifiers = egl
        .query_string(Some(display), egl::EXTENSIONS)
        .is_ok_and(|s| {
            s.to_str()
                .is_ok_and(|s| has_extension(s, "EGL_EXT_image_dma_buf_import_modifiers"))
        });
    // Same calling convention (extern "system"), just typed signatures.
    Some(unsafe {
        DmabufEgl {
            display: display.as_ptr(),
            create_image: std::mem::transmute::<extern "system" fn(), EglCreateImageKhr>(create),
            destroy_image: std::mem::transmute::<extern "system" fn(), EglDestroyImageKhr>(destroy),
            image_target: std::mem::transmute::<extern "system" fn(), GlEglImageTargetTexture2dOes>(
                target,
            ),
            modifiers,
        }
    })
}

/// Whether `list` (a space-separated EGL extension string) contains `want`. Split
/// on whitespace rather than substring-matching, so a longer name that merely
/// starts with `want` doesn't count as a hit.
fn has_extension(list: &str, want: &str) -> bool {
    list.split_ascii_whitespace().any(|e| e == want)
}

/// Build the `EGL_LINUX_DMA_BUF_EXT` attribute list for `frame`. Every plane is
/// described: a compressed modifier carries auxiliary planes, and importing it with
/// only the first one is rejected (`EGL_BAD_MATCH`). The fds are only borrowed (EGL
/// dups them in `eglCreateImageKHR`), so `frame` must outlive the import call.
///
/// `modifiers` gates the modifier attributes: without
/// `EGL_EXT_image_dma_buf_import_modifiers` they are not valid names and would
/// fail the import outright, so the layout is left implicit.
pub(crate) fn dmabuf_image_attribs(frame: &wl::DmabufFrame, modifiers: bool) -> Vec<i32> {
    let mut a = vec![
        EGL_WIDTH,
        frame.width as i32,
        EGL_HEIGHT,
        frame.height as i32,
        EGL_LINUX_DRM_FOURCC_EXT,
        frame.fourcc as i32,
    ];
    for (plane, names) in frame.planes.iter().zip(PLANE_ATTRS) {
        let [fd, offset, pitch, mod_lo, mod_hi] = names;
        a.extend_from_slice(&[
            fd,
            plane.fd.as_raw_fd(),
            offset,
            plane.offset as i32,
            pitch,
            plane.stride as i32,
        ]);
        if modifiers {
            a.extend_from_slice(&[
                mod_lo,
                (frame.modifier & 0xffff_ffff) as i32,
                mod_hi,
                (frame.modifier >> 32) as i32,
            ]);
        }
    }
    a.push(EGL_ATTRIB_NONE);
    a
}

/// Headless EGL/GLES context for reading a capture dma-buf back to CPU RGBA pixels.
///
/// The GPU capture path hands out a [`wl::DmabufFrame`] (zero-copy, meant for
/// display). Tools that ultimately need CPU pixels — screenshot encoding today,
/// video/timelapse on the roadmap — use this to import that dma-buf as a GL texture
/// and `glReadPixels` it into RGBA8. It runs without a window: a 1×1 pbuffer keeps
/// it portable across drivers that lack surfaceless contexts. Build one and reuse it
/// across frames (the EGL setup is not free).
pub struct GpuReadback {
    // Keeps the Wayland display (whose ptr backs the EGL display) alive.
    _conn: Connection,
    egl: Egl,
    display: egl::Display,
    surface: egl::Surface,
    context: egl::Context,
    gl: Arc<glow::Context>,
    dmabuf_egl: Option<DmabufEgl>,
}

impl GpuReadback {
    /// Create the offscreen context. Errors (rather than panicking, unlike
    /// `Gpu::new`) since callers can fall back to the shm capture path.
    pub fn new() -> Result<Self> {
        let conn = Connection::connect_to_env().context("Wayland connection")?;
        let lib = unsafe { egl::DynamicInstance::<egl::EGL1_4>::load_required() }
            .context("libEGL not found")?;
        let egl: Egl = lib;

        let display_ptr = conn.backend().display_ptr() as *mut c_void;
        let display = unsafe { egl.get_display(display_ptr) }.context("eglGetDisplay")?;
        egl.initialize(display).context("eglInitialize")?;
        egl.bind_api(egl::OPENGL_ES_API).context("eglBindAPI")?;

        let attribs = [
            egl::SURFACE_TYPE,
            egl::PBUFFER_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_ES2_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &attribs)
            .context("eglChooseConfig")?
            .context("no EGL pbuffer config")?;

        let ctx_attribs = [egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE];
        let context = egl
            .create_context(display, config, None, &ctx_attribs)
            .or_else(|_| {
                let a = [egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE];
                egl.create_context(display, config, None, &a)
            })
            .context("eglCreateContext")?;

        let pb_attribs = [egl::WIDTH, 1, egl::HEIGHT, 1, egl::NONE];
        let surface = egl
            .create_pbuffer_surface(display, config, &pb_attribs)
            .context("eglCreatePbufferSurface")?;
        egl.make_current(display, Some(surface), Some(surface), Some(context))
            .context("eglMakeCurrent")?;

        let gl = unsafe {
            glow::Context::from_loader_function(|s| {
                egl.get_proc_address(s)
                    .map_or(std::ptr::null(), |p| p as *const _)
            })
        };
        let dmabuf_egl = load_dmabuf_egl(&egl, display);

        Ok(GpuReadback {
            _conn: conn,
            egl,
            display,
            surface,
            context,
            gl: Arc::new(gl),
            dmabuf_egl,
        })
    }

    /// Import `frame`'s dma-buf as a GL texture and read it back to RGBA8. Alpha is
    /// forced opaque: captures are opaque and XRGB dma-bufs leave the X byte
    /// undefined (mirrors the shm path's handling of alpha-less formats).
    pub fn readback(&mut self, frame: wl::DmabufFrame) -> Result<wl::CapturedImage> {
        let egl = self
            .dmabuf_egl
            .context("EGL dma-buf import unavailable (driver)")?;
        let (w, h) = (frame.width, frame.height);
        if w == 0 || h == 0 {
            return Err(CaptureError::msg("zero-sized readback"));
        }

        self.egl
            .make_current(
                self.display,
                Some(self.surface),
                Some(self.surface),
                Some(self.context),
            )
            .context("eglMakeCurrent")?;

        let attribs = dmabuf_image_attribs(&frame, egl.modifiers);
        let image = unsafe {
            (egl.create_image)(
                egl.display,
                std::ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            )
        };
        if image.is_null() {
            return Err(CaptureError::msg("eglCreateImageKHR failed"));
        }

        // Import → bind to a texture → attach to an FBO → glReadPixels. Always
        // destroy the EGLImage afterwards, success or not.
        let read = self.read_image_to_rgba(&egl, image, w, h);
        unsafe { (egl.destroy_image)(egl.display, image) };
        let mut rgba = read?;

        for px in rgba.chunks_exact_mut(4) {
            px[3] = 255;
        }
        Ok(wl::CapturedImage {
            width: w,
            height: h,
            rgba,
        })
    }

    /// Bind `image` to a fresh texture + FBO and read it back, cleaning up the GL
    /// objects before returning. Split out so [`Self::readback`] can destroy the
    /// EGLImage on every path.
    fn read_image_to_rgba(
        &self,
        egl: &DmabufEgl,
        image: EglImage,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        use glow::HasContext as _;
        unsafe {
            let tex = self
                .gl
                .create_texture()
                .map_err(|e| CaptureError::msg(format!("glGenTextures: {e}")))?;
            self.gl.bind_texture(GL_TEXTURE_2D, Some(tex));
            (egl.image_target)(GL_TEXTURE_2D, image);

            let fbo = self.gl.create_framebuffer().map_err(|e| {
                self.gl.delete_texture(tex);
                CaptureError::msg(format!("glGenFramebuffers: {e}"))
            })?;
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                Some(tex),
                0,
            );

            let status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
            let result = if status == glow::FRAMEBUFFER_COMPLETE {
                let mut buf = vec![0u8; w as usize * h as usize * 4];
                self.gl.read_pixels(
                    0,
                    0,
                    w as i32,
                    h as i32,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut buf)),
                );
                Ok(buf)
            } else {
                Err(CaptureError::msg(format!(
                    "incomplete readback FBO (0x{status:x})"
                )))
            };

            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.delete_framebuffer(fbo);
            self.gl.bind_texture(GL_TEXTURE_2D, None);
            self.gl.delete_texture(tex);
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame with `n` planes, each with a distinct offset/stride so the attribute
    /// list can be checked positionally. The fds are dup'd stdin — never imported.
    fn frame(n: usize) -> wl::DmabufFrame {
        let planes = (0..n)
            .map(|i| wl::DmabufPlane {
                fd: rustix::io::dup(std::io::stdin()).unwrap(),
                offset: 100 + i as u32,
                stride: 200 + i as u32,
            })
            .collect();
        wl::DmabufFrame {
            planes,
            width: 64,
            height: 32,
            fourcc: 0x3432_5258,
            modifier: 0x0100_0000_0000_0008,
        }
    }

    /// Attribute lists are name/value pairs; look a name up.
    fn get(attrs: &[i32], name: i32) -> Option<i32> {
        attrs
            .chunks_exact(2)
            .find(|c| c[0] == name)
            .map(|c| c[1])
            .filter(|_| name != EGL_ATTRIB_NONE)
    }

    #[test]
    fn single_plane_attribs() {
        let a = dmabuf_image_attribs(&frame(1), true);
        assert_eq!(get(&a, EGL_WIDTH), Some(64));
        assert_eq!(get(&a, EGL_HEIGHT), Some(32));
        assert_eq!(get(&a, PLANE_ATTRS[0][1]), Some(100));
        assert_eq!(get(&a, PLANE_ATTRS[0][2]), Some(200));
        assert_eq!(a.last(), Some(&EGL_ATTRIB_NONE));
        // No plane 1 entry when there is only one plane.
        assert_eq!(get(&a, PLANE_ATTRS[1][0]), None);
    }

    #[test]
    fn attribs_without_the_modifiers_extension_omit_the_modifier() {
        let a = dmabuf_image_attribs(&frame(1), false);
        assert_eq!(get(&a, PLANE_ATTRS[0][1]), Some(100));
        assert_eq!(get(&a, PLANE_ATTRS[0][3]), None);
        assert_eq!(get(&a, PLANE_ATTRS[0][4]), None);
    }

    #[test]
    fn extension_lookup_matches_whole_names() {
        let list = "EGL_EXT_image_dma_buf_import EGL_KHR_image_base";
        assert!(has_extension(list, "EGL_EXT_image_dma_buf_import"));
        assert!(!has_extension(
            list,
            "EGL_EXT_image_dma_buf_import_modifiers"
        ));
    }

    /// The Intel CCS layouts that issue #6 tripped on carry three planes.
    #[test]
    fn multi_plane_attribs_describe_every_plane() {
        for n in 2..=4 {
            let a = dmabuf_image_attribs(&frame(n), true);
            for (i, names) in PLANE_ATTRS.iter().enumerate().take(n) {
                assert_eq!(get(&a, names[1]), Some(100 + i as i32), "plane {i}");
                assert_eq!(get(&a, names[2]), Some(200 + i as i32), "plane {i}");
                // The modifier is repeated on every plane, split in halves.
                assert_eq!(get(&a, names[3]), Some(8), "plane {i}");
                assert_eq!(get(&a, names[4]), Some(0x0100_0000), "plane {i}");
            }
            assert!(get(&a, PLANE_ATTRS[n - 1][0]).is_some());
            assert_eq!(a.last(), Some(&EGL_ATTRIB_NONE));
        }
    }
}
