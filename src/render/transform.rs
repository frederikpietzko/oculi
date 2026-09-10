//! Pure geometry in top-left pixel coordinates; no device or UI dependencies.

use crate::model::{Resolution, ViewportTransform};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geometry {
    pub crop: Rect,
    pub placement: Rect,
}

pub fn validate(transform: &ViewportTransform) -> Result<(), String> {
    if !transform.is_crop_valid() {
        return Err("Crop must be finite, nonempty, and inside the source".into());
    }
    if !transform.scale.is_finite() || transform.scale <= 0.0 || transform.scale > 100.0 {
        return Err("Viewport scale must be finite and in (0, 100]".into());
    }
    if [transform.offset_x, transform.offset_y]
        .iter()
        .any(|v| !v.is_finite() || v.abs() > 100.0)
    {
        return Err("Viewport offsets must be finite and in [-100, 100]".into());
    }
    Ok(())
}

pub fn geometry(
    transform: ViewportTransform,
    source: Resolution,
    canvas: Resolution,
) -> Result<Geometry, String> {
    validate(&transform)?;
    if source.width == 0 || source.height == 0 || canvas.width == 0 || canvas.height == 0 {
        return Err("Source and canvas dimensions must be nonzero".into());
    }
    let crop = Rect {
        x: transform.crop_x * f64::from(source.width),
        y: transform.crop_y * f64::from(source.height),
        width: transform.crop_width * f64::from(source.width),
        height: transform.crop_height * f64::from(source.height),
    };
    let cw = f64::from(canvas.width);
    let ch = f64::from(canvas.height);
    let scale = (cw / crop.width).min(ch / crop.height) * transform.scale;
    let width = crop.width * scale;
    let height = crop.height * scale;
    let placement = Rect {
        x: (cw - width) / 2.0 + transform.offset_x * cw,
        y: (ch - height) / 2.0 + transform.offset_y * ch,
        width,
        height,
    };
    if !scale.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err("Crop/scale is too small to represent".into());
    }
    Ok(Geometry { crop, placement })
}

/// Centered aspect fit, preserving the normalized crop.
pub fn fit(transform: &mut ViewportTransform) {
    transform.offset_x = 0.0;
    transform.offset_y = 0.0;
    transform.scale = 1.0;
}

/// Centered aspect fill, preserving the crop. Invalid inputs leave it unchanged.
pub fn fill(
    transform: &mut ViewportTransform,
    source: Resolution,
    canvas: Resolution,
) -> Result<(), String> {
    let mut next = *transform;
    fit(&mut next);
    let g = geometry(next, source, canvas)?;
    next.scale = (f64::from(canvas.width) / g.placement.width)
        .max(f64::from(canvas.height) / g.placement.height);
    validate(&next)?;
    *transform = next;
    Ok(())
}

/// Round AppKit points to backing pixels exactly once, independent of framing.
pub fn backing_size(width: f64, height: f64, scale: f64) -> Result<Resolution, String> {
    if [width, height, scale]
        .iter()
        .any(|v| !v.is_finite() || *v <= 0.0)
    {
        return Err("Preview size and backing scale must be finite and positive".into());
    }
    let width = (width * scale).round();
    let height = (height * scale).round();
    if !(1.0..=16384.0).contains(&width) || !(1.0..=16384.0).contains(&height) {
        return Err("Preview backing dimensions must be in 1..=16384".into());
    }
    Ok(Resolution {
        width: width as u32,
        height: height as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(width: u32, height: u32) -> Resolution {
        Resolution { width, height }
    }

    #[test]
    fn default_fits_and_fill_preserves_crop() {
        let mut t = ViewportTransform::default();
        let g = geometry(t, size(200, 100), size(100, 100)).unwrap();
        assert_eq!(
            g.placement,
            Rect {
                x: 0.0,
                y: 25.0,
                width: 100.0,
                height: 50.0
            }
        );
        fill(&mut t, size(200, 100), size(100, 100)).unwrap();
        assert_eq!(t.scale, 2.0);
        assert_eq!(
            geometry(t, size(200, 100), size(100, 100))
                .unwrap()
                .placement
                .x,
            -50.0
        );
        t.crop_x = 0.25;
        t.crop_width = 0.5;
        fit(&mut t);
        fill(&mut t, size(200, 100), size(100, 100)).unwrap();
        assert_eq!(t.crop_x, 0.25);
        assert_eq!(t.crop_width, 0.5);
        assert_eq!(t.scale, 1.0);
    }

    #[test]
    fn crop_resize_translation_and_independent_framing() {
        let a = ViewportTransform {
            crop_x: 0.25,
            crop_y: 0.25,
            crop_width: 0.5,
            crop_height: 0.5,
            offset_x: 0.1,
            offset_y: -0.2,
            scale: 2.0,
        };
        let mut b = a;
        fit(&mut b);
        let g = geometry(a, size(400, 200), size(200, 200)).unwrap();
        assert_eq!(
            g.crop,
            Rect {
                x: 100.0,
                y: 50.0,
                width: 200.0,
                height: 100.0
            }
        );
        assert_eq!(
            g.placement,
            Rect {
                x: -80.0,
                y: -40.0,
                width: 400.0,
                height: 200.0
            }
        );
        assert_eq!(
            geometry(a, size(800, 400), size(200, 200))
                .unwrap()
                .placement,
            g.placement
        );
        assert_eq!(a.scale, 2.0);
        assert_eq!(b.scale, 1.0);
    }

    #[test]
    fn rejects_bad_geometry_and_retina_does_not_change_transform() {
        for value in [f64::NAN, f64::INFINITY, -1.0, 0.0, 101.0] {
            let t = ViewportTransform {
                scale: value,
                ..Default::default()
            };
            assert!(validate(&t).is_err());
        }
        for t in [
            ViewportTransform {
                crop_width: f64::NAN,
                ..Default::default()
            },
            ViewportTransform {
                crop_x: 0.1,
                ..Default::default()
            },
            ViewportTransform {
                crop_height: 0.0,
                ..Default::default()
            },
            ViewportTransform {
                offset_y: f64::INFINITY,
                ..Default::default()
            },
            ViewportTransform {
                offset_x: -101.0,
                ..Default::default()
            },
        ] {
            assert!(validate(&t).is_err());
        }
        assert!(geometry(Default::default(), size(0, 100), size(100, 100)).is_err());
        assert_eq!(backing_size(200.0, 100.0, 2.0).unwrap(), size(400, 200));
        assert!(backing_size(f64::NAN, 100.0, 2.0).is_err());
    }
}
