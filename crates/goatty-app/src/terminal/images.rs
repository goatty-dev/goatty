//! GPU textures for a pane's images (kitty graphics, sixel, iTerm2).
//!
//! Each [`goatty_term::Image`] becomes one `RenderImage` the first time it's
//! on screen, and its texture leaves the window's atlas as soon as a frame
//! no longer shows it: programs like terminal browsers send a new image for
//! every frame, so keeping old ones would fill the atlas.

use std::collections::HashMap;
use std::sync::Arc;

use goatty_term::Image;
use gpui::{App, RenderImage, Window};

/// Larger images are scaled down to this many pixels on their longer side
/// (a texture has to fit the atlas).
const MAX_SIDE: u32 = 4096;

#[derive(Default)]
pub struct ImageTextures {
    textures: HashMap<u64, Entry>,
    frame: u64,
}

struct Entry {
    texture: Arc<RenderImage>,
    used: u64,
}

impl ImageTextures {
    /// Start a frame: what isn't asked for until [`Self::sweep`] goes.
    pub fn begin(&mut self) {
        self.frame += 1;
    }

    /// The texture for `image`, made on first use.
    pub fn get(&mut self, image: &Image) -> Option<Arc<RenderImage>> {
        let frame = self.frame;
        if let Some(e) = self.textures.get_mut(&image.id) {
            e.used = frame;
            return Some(e.texture.clone());
        }
        let texture = Arc::new(texture(image)?);
        self.textures.insert(image.id, Entry { texture: texture.clone(), used: frame });
        Some(texture)
    }

    /// Free the textures this frame didn't use.
    pub fn sweep(&mut self, window: &mut Window, cx: &mut App) {
        let frame = self.frame;
        if self.textures.values().all(|e| e.used == frame) {
            return;
        }
        let mut gone = Vec::new();
        self.textures.retain(|_, e| {
            let keep = e.used == frame;
            if !keep {
                gone.push(e.texture.clone());
            }
            keep
        });
        for texture in gone {
            cx.drop_image(texture, Some(window));
        }
    }

    /// Free every texture (the pane is going away).
    pub fn clear(&mut self, cx: &mut App) {
        for (_, e) in self.textures.drain() {
            cx.drop_image(e.texture, None);
        }
    }
}

/// BGRA pixels for gpui, scaled down when too large.
fn texture(image: &Image) -> Option<RenderImage> {
    let mut buf = image::RgbaImage::from_raw(image.width, image.height, image.rgba.clone())?;
    if image.width > MAX_SIDE || image.height > MAX_SIDE {
        let scale = MAX_SIDE as f32 / image.width.max(image.height) as f32;
        let (w, h) = (((image.width as f32 * scale) as u32).max(1), ((image.height as f32 * scale) as u32).max(1));
        buf = image::imageops::resize(&buf, w, h, image::imageops::FilterType::Triangle);
    }
    for px in buf.as_chunks_mut::<4>().0 {
        px.swap(0, 2);
    }
    Some(RenderImage::new(vec![image::Frame::new(buf)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(id: u64, w: u32, h: u32) -> Image {
        Image { id, width: w, height: h, rgba: [10, 20, 30, 40].repeat((w * h) as usize) }
    }

    #[test]
    fn textures_are_bgra_and_made_once() {
        let mut t = ImageTextures::default();
        t.begin();
        let a = t.get(&image(1, 2, 2)).unwrap();
        let b = t.get(&image(1, 2, 2)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(&a.as_bytes(0).unwrap()[..4], &[30, 20, 10, 40]);
    }

    #[test]
    fn huge_images_are_scaled_to_fit() {
        let t = texture(&image(1, 8192, 16)).unwrap();
        let s = t.size(0);
        assert_eq!((s.width.0, s.height.0), (4096, 8));
    }

    #[test]
    fn a_broken_image_has_no_texture() {
        let bad = Image { id: 1, width: 10, height: 10, rgba: vec![0; 3] };
        assert!(ImageTextures::default().get(&bad).is_none());
    }
}
