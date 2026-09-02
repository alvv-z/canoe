//! Highlight overlay previewing the area a dragged window will snap to.

use memmap2::MmapMut;
use std::fs::File;
use std::os::fd::AsFd;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::QueueHandle;
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::ZwlrLayerSurfaceV1;

use super::{shmfile, OutputId};

/// Semi-transparent rectangle previewing a snap target area. Sized to the
/// target area (not the output) and purely decorative: the input region is
/// always empty so it never interferes with the drag.
pub struct SnapPreview {
    pub surface: wl_surface::WlSurface,
    pub layer_surface: ZwlrLayerSurfaceV1,
    pub buffer: Option<wl_buffer::WlBuffer>,
    pub pool: Option<wl_shm_pool::WlShmPool>,
    pub memfile: Option<File>,
    pub mmap: Option<MmapMut>,
    pub width: i32,
    pub height: i32,
    pub configured: bool,
    pub output_id: OutputId,
    pub dirty: bool,
}

impl SnapPreview {
    pub fn new(
        surface: wl_surface::WlSurface,
        layer_surface: ZwlrLayerSurfaceV1,
        output_id: OutputId,
    ) -> Self {
        Self {
            surface,
            layer_surface,
            buffer: None,
            pool: None,
            memfile: None,
            mmap: None,
            width: 0,
            height: 0,
            configured: false,
            output_id,
            dirty: true,
        }
    }

    pub fn configure(&mut self, width: i32, height: i32) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.configured = true;
        self.dirty = true;
    }

    pub fn reset_buffer(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        if let Some(pool) = self.pool.take() {
            pool.destroy();
        }
        self.memfile = None;
        self.mmap = None;
        self.dirty = true;
    }

    pub fn ensure_buffer<D>(&mut self, shm: &wl_shm::WlShm, qh: &QueueHandle<D>)
    where
        D: 'static
            + wayland_client::Dispatch<wl_shm_pool::WlShmPool, ()>
            + wayland_client::Dispatch<wl_buffer::WlBuffer, ()>,
    {
        if self.width <= 0 || self.height <= 0 {
            return;
        }
        if self.buffer.is_some() {
            return;
        }

        let stride = self.width * 4;
        let size = stride * self.height;
        let memfile = match shmfile::create("canoe-snap-preview", size as i64) {
            Ok(f) => f,
            Err(_) => return,
        };
        let mmap = match unsafe { memmap2::MmapMut::map_mut(&memfile) } {
            Ok(m) => m,
            Err(_) => return,
        };

        let pool = shm.create_pool(memfile.as_fd(), size, qh, ());
        let buffer = pool.create_buffer(
            0,
            self.width,
            self.height,
            stride,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );

        self.memfile = Some(memfile);
        self.mmap = Some(mmap);
        self.pool = Some(pool);
        self.buffer = Some(buffer);
    }

    /// Fill with a translucent color (0xRRGGBBAA, premultiplied on write).
    pub fn render(&mut self, rgba: u32) {
        if let Some(ref mut mmap) = self.mmap {
            let r = (rgba >> 24) & 0xff;
            let g = (rgba >> 16) & 0xff;
            let b = (rgba >> 8) & 0xff;
            let a = rgba & 0xff;
            let argb = (a << 24) | (r << 16) | (g << 8) | b;
            let bytes = argb.to_ne_bytes();
            for chunk in mmap.as_mut().chunks_exact_mut(4) {
                chunk.copy_from_slice(&bytes);
            }
        }
    }

    /// Always empty: the preview must never intercept pointer input.
    pub fn update_input_region<D>(
        &self,
        compositor: &wl_compositor::WlCompositor,
        qh: &QueueHandle<D>,
    ) where
        D: 'static + wayland_client::Dispatch<wl_region::WlRegion, ()>,
    {
        let region = compositor.create_region(qh, ());
        self.surface.set_input_region(Some(&region));
        region.destroy();
    }

    pub fn commit(&self) {
        if let Some(ref buffer) = self.buffer {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface.damage_buffer(0, 0, self.width, self.height);
            self.surface.commit();
        }
    }
}

impl Drop for SnapPreview {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        if let Some(pool) = self.pool.take() {
            pool.destroy();
        }
        self.layer_surface.destroy();
        self.surface.destroy();
    }
}
