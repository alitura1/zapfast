//! The chat wallpaper: a colour with the default doodles over it.

use std::sync::{Arc, Mutex};

use egui::{Color32, ColorImage, Rect, TextureHandle, TextureOptions, pos2};

/// The default doodle tile, drawn from Lucide icons (ISC, see
/// `assets/icons/LICENSE.txt`). It is embedded so the wallpaper works offline
/// and does not depend on a third-party request at runtime.
const DEFAULT_SVG: &[u8] = include_bytes!("../assets/wallpaper.svg");

#[derive(Clone, Default)]
struct Cache(Arc<Mutex<Option<TextureHandle>>>);

/// What the conversation shows behind its bubbles.
#[derive(Clone)]
pub struct Look {
    /// The background colour, with the Theme choice already resolved.
    pub color: Color32,
    /// Whether the doodles are drawn over the colour.
    pub doodles: bool,
}

/// Paint the wallpaper over the conversation panel.
pub fn paint(ui: &egui::Ui, look: &Look) {
    paint_rect(ui, ui.max_rect(), look);
}

/// Paint the wallpaper into a bounded rectangle: the colour with the SVG's
/// intrinsic 374 x 666 logical-pixel doodle tile repeated in both axes.
pub fn paint_rect(ui: &egui::Ui, rect: Rect, look: &Look) {
    let painter = ui.painter().with_clip_rect(rect);
    painter.rect_filled(rect, 0.0, look.color);

    if !look.doodles {
        return;
    }

    let Some(texture) = texture(ui.ctx()) else {
        return;
    };

    let tile = texture.size_vec2();
    if tile.x <= 0.0 || tile.y <= 0.0 {
        return;
    }

    let line = doodle_ink(look.color);
    let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
    let origin = pos2(0.0, 0.0);
    let first_x = origin.x + (rect.left() - origin.x).div_euclid(tile.x) * tile.x;
    let first_y = origin.y + (rect.top() - origin.y).div_euclid(tile.y) * tile.y;

    let mut y = first_y;
    while y < rect.bottom() {
        let mut x = first_x;
        while x < rect.right() {
            let tile_rect = Rect::from_min_size(pos2(x, y), tile);
            if tile_rect.intersects(rect) {
                painter.image(texture.id(), tile_rect, uv, line);
            }
            x += tile.x;
        }
        y += tile.y;
    }
}

/// The doodles' tint over `background`: whichever of near-black or white
/// stands further from it, as opaque as it takes to differ from the
/// background by a fixed step. Any theme colour then shows the doodles about
/// as faintly as WhatsApp's own colours do, mid-tones included.
pub fn doodle_ink(background: Color32) -> Color32 {
    const DARK_INK: u8 = 30;
    const LIGHT_INK: u8 = 255;
    let luma = 0.2126 * f32::from(background.r())
        + 0.7152 * f32::from(background.g())
        + 0.0722 * f32::from(background.b());
    let (ink, step) = if luma - f32::from(DARK_INK) > f32::from(LIGHT_INK) - luma {
        (DARK_INK, 23.2)
    } else {
        (LIGHT_INK, 38.0)
    };
    let distance = (luma - f32::from(ink)).abs().max(1.0);
    let alpha = (step / distance).clamp(0.1, 0.4);
    Color32::from_rgba_unmultiplied(ink, ink, ink, (alpha * 255.0).round() as u8)
}

/// The doodle tile's texture, for finding it in painted shapes.
#[cfg(test)]
pub(crate) fn doodle_texture_id(ctx: &egui::Context) -> egui::TextureId {
    texture(ctx).expect("the doodle tile renders").id()
}

fn texture(ctx: &egui::Context) -> Option<TextureHandle> {
    let cache = ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Cache>(egui::Id::new("chat-wallpaper"))
            .clone()
    });
    let mut cached = cache
        .0
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(texture) = cached.as_ref() {
        return Some(texture.clone());
    }

    let image = rasterize()?;
    let texture = ctx.load_texture("chat-wallpaper", image, TextureOptions::LINEAR);
    *cached = Some(texture.clone());
    Some(texture)
}

fn rasterize() -> Option<ColorImage> {
    // The source uses currentColor. Render a white mask once, then use the
    // painter tint to adapt the line colour and opacity to the active theme.
    let source = String::from_utf8_lossy(DEFAULT_SVG).replace("currentColor", "#ffffff");
    let tree =
        resvg::usvg::Tree::from_data(source.as_bytes(), &resvg::usvg::Options::default()).ok()?;
    let size = tree.size();
    let width = size.width().round() as u32;
    let height = size.height().round() as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect::<Vec<u8>>();
    Some(ColorImage::from_rgba_unmultiplied(
        [width as usize, height as usize],
        &rgba,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_tile_keeps_its_intrinsic_dimensions() {
        let image = rasterize().expect("default wallpaper SVG renders");
        assert_eq!(image.size, [374, 666]);
        assert!(image.pixels.iter().any(|pixel| pixel.a() != 0));
    }

    #[test]
    fn doodles_show_on_every_background() {
        for level in 0..=255u8 {
            for background in [
                Color32::from_rgb(level, level, level),
                Color32::from_rgb(level, 0, 0),
                Color32::from_rgb(0, level, level / 2),
            ] {
                let ink = doodle_ink(background).to_srgba_unmultiplied();
                let alpha = f32::from(ink[3]) / 255.0;
                let over = |channel: u8, ink: u8| {
                    f32::from(channel) * (1.0 - alpha) + f32::from(ink) * alpha
                };
                let luma = |r: f32, g: f32, b: f32| 0.2126 * r + 0.7152 * g + 0.0722 * b;
                let before = luma(
                    f32::from(background.r()),
                    f32::from(background.g()),
                    f32::from(background.b()),
                );
                let after = luma(
                    over(background.r(), ink[0]),
                    over(background.g(), ink[1]),
                    over(background.b(), ink[2]),
                );
                assert!(
                    (after - before).abs() >= 20.0,
                    "doodles fade into {background:?}: {before} to {after}"
                );
            }
        }
        // WhatsApp's own light and dark defaults keep their familiar look.
        assert_eq!(ink_alpha(doodle_ink(Color32::from_rgb(245, 241, 235))), 28);
        assert_eq!(ink_alpha(doodle_ink(Color32::from_rgb(22, 23, 23))), 42);
    }

    fn ink_alpha(ink: Color32) -> u8 {
        ink.to_srgba_unmultiplied()[3]
    }
}
