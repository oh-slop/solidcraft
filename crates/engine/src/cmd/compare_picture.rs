//! Compare the model with a reference picture (#1486): agents model to a picture, and an
//! overlap score plus per-edge mm offsets tell the agent which edge to move and by how much.

use std::collections::VecDeque;
use std::io::Cursor;

use serde_json::{Value, json};
use solidcraft_doc::canvas::base64_encode;

use super::CommandSpec;
use super::silhouette::{self, Silhouette};
use crate::params::bad;
use crate::{Result, Session};

const CMD: &str = "inspect.compare_picture";
/// A reference picture is read through the vfs with a byte cap, like `screenshot` output.
const MAX_REFERENCE_BYTES: usize = 32 << 20;
/// Decoded pixels are capped as well, so a small file cannot unfold into a huge raster.
const MAX_DECODED_PIXELS: u64 = 16 << 20;
/// The comparison grid each mask is fitted into at most; keeps the slide search bounded.
const MAX_GRID: usize = 1024;
/// Background grows from the border only through steps of at most this per channel, so a
/// backdrop's gradual shading belongs to it and the step at the part's outline stops it.
const BG_STEP: u16 = 12;
/// Stations per side for the edge offsets.
const STATIONS: usize = 9;

// For agents modelling to a picture; nothing to show a person, so no toolbar button.
pub static COMMANDS: &[CommandSpec] = &[CommandSpec::new(CMD, "Compare Picture", run).noundo().icon("compare").params(
    "reference: path to a PNG or JPEG the model should match; view?/camera?/bodies?/rows? as inspect.silhouette; \
     crop?: [x, y, w, h] in pixels, or fractions of the picture when all four are 0..1 (to take one view off a board); \
     fit?: \"width\" | \"height\" | \"both\" (default \"both\"): the scale comes from the dimension(s) the two share; \
     overlay?: false leaves the overlay PNG out → overlap (0..1), edges: top|bottom|left|right in mm at nine stations \
     (positive where the model reaches past the picture, null where a station misses one of the two), the grid and its \
     mm_per_px, and overlay_png (red model, blue picture) as base64 PNG",
)];

fn run(s: &mut Session, p: &Value) -> Result<Value> {
    let reference = p
        .get("reference")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| bad(CMD, "`reference` is required: a path to a PNG or JPEG"))?;
    let fit = match p.get("fit") {
        None => Fit::Both,
        Some(Value::String(v)) => match v.as_str() {
            "width" => Fit::Width,
            "height" => Fit::Height,
            "both" => Fit::Both,
            other => return Err(bad(CMD, format!("`fit` must be \"width\", \"height\" or \"both\", not `{other}`"))),
        },
        Some(_) => return Err(bad(CMD, "`fit` must be a string")),
    };
    let overlay = p.get("overlay").map(|v| v.as_bool().unwrap_or(false)).unwrap_or(true);

    // The model side: the same outline inspect.silhouette returns, as filled rows.
    let camera = silhouette::camera(p)?;
    let rows = silhouette::rows(p)?;
    let wanted = silhouette::body_names(p)?;
    let model = silhouette::outline(s, &camera, wanted.as_deref(), rows)?;

    // The picture side: decode, crop, and find the part.
    let bytes = solidcraft_io::vfs::read(reference, MAX_REFERENCE_BYTES).map_err(|e| bad(CMD, format!("{reference}: {e}")))?;
    let img = decode(&bytes)?;
    let img = apply_crop(img, p.get("crop"))?;
    let picture =
        find_part(&img).ok_or_else(|| bad(CMD, "no part found in the picture: the background grew from every border and covered it all"))?;

    // Fit the model outline to the picture's scale, then slide it to the best overlap.
    let (mw, mh, mmpp_x, mmpp_y) = fit_model(&model, picture.width, picture.height, fit);
    let model_scaled = resample(&model.mask, model.width, model.height, mw, mh);
    let best = best_overlap(&model_scaled, &picture);
    let placed = Placed { dx: best.dx, dy: best.dy, mask: &model_scaled };

    let mut out = json!({
        "overlap": round4(best.overlap),
        "stations": STATIONS,
        "edges": {
            "top": edge_offsets(&placed, &picture, Edge::Top, mmpp_y),
            "bottom": edge_offsets(&placed, &picture, Edge::Bottom, mmpp_y),
            "left": edge_offsets(&placed, &picture, Edge::Left, mmpp_x),
            "right": edge_offsets(&placed, &picture, Edge::Right, mmpp_x),
        },
        "fit": match fit { Fit::Width => "width", Fit::Height => "height", Fit::Both => "both" },
        "mm_per_px": {"x": round4(mmpp_x), "y": round4(mmpp_y)},
        "model": {"width": model.width, "height": model.height, "box_mm": [[round4(model.lo[0]), round4(model.lo[1])], [round4(model.hi[0]), round4(model.hi[1])]], "bodies": model.bodies},
        "picture": {"width": picture.width, "height": picture.height, "source": reference},
    });
    if overlay {
        out["overlay_png"] = json!(base64_encode(&overlay_png(&picture, &placed)));
    }
    Ok(out)
}

#[derive(Clone, Copy)]
enum Fit {
    Width,
    Height,
    Both,
}

#[derive(Clone, Copy)]
enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// A picture mask, tightly framed to the found part.
struct Part {
    width: usize,
    height: usize,
    mask: Vec<Vec<bool>>,
}

/// A model mask placed relative to the picture: its top-left sits at (dx, dy) in the
/// picture's own pixel coordinates, which may be negative.
struct Placed<'a> {
    dx: i64,
    dy: i64,
    mask: &'a [Vec<bool>],
}

/// Decode PNG or JPEG to RGBA8, refusing anything else and anything that unfolds too large.
fn decode(bytes: &[u8]) -> Result<image::RgbaImage> {
    let guess = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| bad(CMD, format!("cannot read the picture: {e}")))?;
    if !matches!(guess.format(), Some(image::ImageFormat::Png) | Some(image::ImageFormat::Jpeg)) {
        return Err(bad(CMD, "the reference must be a PNG or a JPEG"));
    }
    let (w, h) = guess.into_dimensions().map_err(|e| bad(CMD, format!("cannot read the picture: {e}")))?;
    if w as u64 * h as u64 > MAX_DECODED_PIXELS {
        return Err(bad(CMD, format!("the picture is {w}x{h}: over the {} megapixel limit", MAX_DECODED_PIXELS >> 20)));
    }
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format().map_err(|e| bad(CMD, format!("cannot read the picture: {e}")))?;
    let img = reader.decode().map_err(|e| bad(CMD, format!("cannot decode the picture: {e}")))?;
    Ok(img.to_rgba8())
}

/// Apply `crop: [x, y, w, h]` — pixels, or fractions of the picture when all four are 0..1.
fn apply_crop(img: image::RgbaImage, crop: Option<&Value>) -> Result<image::RgbaImage> {
    let Some(crop) = crop else { return Ok(img) };
    let nums: Vec<f64> = crop
        .as_array()
        .filter(|a| a.len() == 4)
        .and_then(|a| a.iter().map(|v| v.as_f64()).collect())
        .ok_or_else(|| bad(CMD, "`crop` must be [x, y, w, h] in pixels or fractions of the picture"))?;
    if nums.iter().any(|v| !v.is_finite() || *v < 0.0) {
        return Err(bad(CMD, "`crop` values must be non-negative"));
    }
    let fractional = nums.iter().all(|v| *v <= 1.0);
    let (w, h) = img.dimensions();
    let px = |v: f64, full: u32| -> usize {
        let n = if fractional { v * full as f64 } else { v };
        n.round().clamp(0.0, full as f64) as usize
    };
    let x = px(nums[0], w);
    let y = px(nums[1], h);
    if x >= w as usize || y >= h as usize {
        return Err(bad(CMD, "`crop` starts outside the picture"));
    }
    let cw = px(nums[2], w).max(1).min(w as usize - x);
    let ch = px(nums[3], h).max(1).min(h as usize - y);
    Ok(image::imageops::crop_imm(&img, x as u32, y as u32, cw as u32, ch as u32).to_image())
}

/// Find the part: grow the background in from the border through small colour steps only
/// (so a backdrop's gradual shading belongs to it and the step at the part's outline stops
/// it), keep what the background did not reach — holes inside the part count as the part —
/// and keep the largest 4-connected piece.
fn find_part(img: &image::RgbaImage) -> Option<Part> {
    let (w, h) = (img.width() as usize, img.height() as usize);
    if w < 2 || h < 2 {
        return None;
    }
    let px = |x: usize, y: usize| -> [u16; 3] {
        let p = img.get_pixel(x as u32, y as u32).0;
        [p[0] as u16, p[1] as u16, p[2] as u16]
    };
    let step = |a: [u16; 3], b: [u16; 3]| (a[0].abs_diff(b[0])).max(a[1].abs_diff(b[1])).max(a[2].abs_diff(b[2]));

    // Background: flood from every border pixel, crossing only small colour steps.
    let mut bg = vec![false; w * h];
    let mut queue: VecDeque<(usize, usize)> = VecDeque::new();
    for x in 0..w {
        for y in [0, h - 1] {
            if !bg[y * w + x] {
                bg[y * w + x] = true;
                queue.push_back((x, y));
            }
        }
    }
    for y in 0..h {
        for x in [0, w - 1] {
            if !bg[y * w + x] {
                bg[y * w + x] = true;
                queue.push_back((x, y));
            }
        }
    }
    while let Some((x, y)) = queue.pop_front() {
        let here = px(x, y);
        for (nx, ny) in neighbours(x, y, w, h) {
            if !bg[ny * w + nx] && step(here, px(nx, ny)) <= BG_STEP {
                bg[ny * w + nx] = true;
                queue.push_back((nx, ny));
            }
        }
    }

    // The part is what the background did not reach; keep its largest 4-connected piece.
    let part: Vec<bool> = (0..w * h).map(|i| !bg[i]).collect();
    let mut best: Vec<usize> = Vec::new();
    let mut seen = vec![false; w * h];
    for start in 0..w * h {
        if !part[start] || seen[start] {
            continue;
        }
        let mut piece: Vec<usize> = Vec::new();
        let mut queue: VecDeque<usize> = VecDeque::new();
        seen[start] = true;
        queue.push_back(start);
        while let Some(i) = queue.pop_front() {
            piece.push(i);
            let (x, y) = (i % w, i / w);
            for (nx, ny) in neighbours(x, y, w, h) {
                let j = ny * w + nx;
                if part[j] && !seen[j] {
                    seen[j] = true;
                    queue.push_back(j);
                }
            }
        }
        if piece.len() > best.len() {
            best = piece;
        }
    }
    if best.len() < 16 {
        return None;
    }
    // Tight frame, then downscale to the comparison grid at need.
    let (x0, y0, x1, y1) = frame(&best, w);
    let (pw, ph) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut mask = vec![vec![false; pw]; ph];
    for &i in &best {
        let (x, y) = (i % w, i / w);
        mask[y - y0][x - x0] = true;
    }
    let (dw, dh) = fit_grid(pw, ph);
    let mask = if (dw, dh) == (pw, ph) { mask } else { resample(&mask, pw, ph, dw, dh) };
    Some(Part { width: dw, height: dh, mask })
}

fn neighbours(x: usize, y: usize, w: usize, h: usize) -> impl Iterator<Item = (usize, usize)> {
    [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)].into_iter().filter(move |(nx, ny)| *nx < w && *ny < h)
}

fn frame(pixels: &[usize], w: usize) -> (usize, usize, usize, usize) {
    let mut x0 = usize::MAX;
    let mut y0 = usize::MAX;
    let mut x1 = 0;
    let mut y1 = 0;
    for &i in pixels {
        let (x, y) = (i % w, i / w);
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    (x0, y0, x1, y1)
}

fn fit_grid(w: usize, h: usize) -> (usize, usize) {
    let scale = (MAX_GRID as f64 / w.max(h) as f64).min(1.0);
    (((w as f64 * scale).round() as usize).max(2), ((h as f64 * scale).round() as usize).max(2))
}

/// Nearest-neighbour resample of a mask to (w1, h1).
fn resample(mask: &[Vec<bool>], w0: usize, h0: usize, w1: usize, h1: usize) -> Vec<Vec<bool>> {
    (0..h1)
        .map(|y| {
            let row = &mask[((y * h0) / h1.max(1)).min(h0 - 1)];
            (0..w1).map(|x| row[((x * w0) / w1.max(1)).min(w0 - 1)]).collect()
        })
        .collect()
}

/// The model outline's size in picture pixels and the mm per pixel that follows, from the
/// dimension(s) the two share.
fn fit_model(model: &Silhouette, pw: usize, ph: usize, fit: Fit) -> (usize, usize, f64, f64) {
    let (sx, sy) = match fit {
        Fit::Width => {
            let s = pw as f64 / model.width as f64;
            (s, s)
        }
        Fit::Height => {
            let s = ph as f64 / model.height as f64;
            (s, s)
        }
        Fit::Both => (pw as f64 / model.width as f64, ph as f64 / model.height as f64),
    };
    let mw = ((model.width as f64 * sx).round() as usize).max(1);
    let mh = ((model.height as f64 * sy).round() as usize).max(1);
    (mw, mh, model.mm_per_px / sx, model.mm_per_px / sy)
}

struct Best {
    overlap: f64,
    dx: i64,
    dy: i64,
}

/// Slide the model mask over the picture mask to the best intersection-over-union. The
/// search is coarse (step 8) then fine (step 1) within a margin of a tenth of the picture
/// around the centred start — where a picture the model was modelled from lands.
fn best_overlap(model: &[Vec<bool>], picture: &Part) -> Best {
    let margin = (picture.width.max(picture.height) / 10).clamp(8, 128) as i64;
    let pic = spans_of(&picture.mask);
    let mdl = spans_of(model);
    let model_area: i64 = mdl.iter().flatten().map(|[a, b]| (b - a) as i64).sum();
    let pic_area: i64 = pic.iter().flatten().map(|[a, b]| (b - a) as i64).sum();
    let centre_x = ((picture.width as i64 - model[0].len() as i64) / 2).max(0);
    let centre_y = ((picture.height as i64 - model.len() as i64) / 2).max(0);

    let (mut bx, mut by) = (centre_x, centre_y);
    let (mw, mh) = (model[0].len() as i64, model.len() as i64);
    for step in [8i64, 1] {
        // Coarse: a tenth of the picture around the start, every 8th offset. Fine: ±8 around
        // the coarse winner, every offset.
        let half = if step == 8 { margin } else { 8 };
        let (lo_x, hi_x) = ((bx - half).max(1 - mw), (bx + half).min(picture.width as i64));
        let (lo_y, hi_y) = ((by - half).max(1 - mh), (by + half).min(picture.height as i64));
        let (mut best_score, mut best_x, mut best_y) = (i64::MIN, bx, by);
        let mut dy = lo_y;
        while dy <= hi_y {
            let mut dx = lo_x;
            while dx <= hi_x {
                let hits = intersection(&mdl, &pic, dx, dy);
                let union = model_area + pic_area - hits;
                let score = if union <= 0 { i64::MIN } else { hits * 1_000_000 / union };
                if score > best_score {
                    best_score = score;
                    best_x = dx;
                    best_y = dy;
                }
                dx += step;
            }
            dy += step;
        }
        bx = best_x;
        by = best_y;
    }

    let hits = intersection(&mdl, &pic, bx, by);
    let union = model_area + pic_area - hits;
    Best { overlap: if union <= 0 { 0.0 } else { hits as f64 / union as f64 }, dx: bx, dy: by }
}

fn spans_of(mask: &[Vec<bool>]) -> Vec<Vec<[usize; 2]>> {
    mask.iter().map(|row| silhouette::spans(row)).collect()
}

/// Pixels the model shares with the picture at model offset (dx, dy), from run-length rows.
fn intersection(mdl: &[Vec<[usize; 2]>], pic: &[Vec<[usize; 2]>], dx: i64, dy: i64) -> i64 {
    let mut hits = 0i64;
    for (r, spans) in mdl.iter().enumerate() {
        if spans.is_empty() {
            continue;
        }
        let Some(y) = (dy + r as i64).try_into().ok().and_then(|y: usize| pic.get(y)) else { continue };
        if y.is_empty() {
            continue;
        }
        for &[a, b] in spans {
            let (mut lo, hi) = ((a as i64 + dx).max(0), b as i64 + dx);
            if hi <= lo {
                continue;
            }
            for &[pa, pb] in y {
                let (s, e) = (lo.max(pa as i64), hi.min(pb as i64));
                if e > s {
                    hits += e - s;
                }
                lo = lo.max(pb as i64);
                if lo >= hi || pb as i64 >= hi {
                    break;
                }
            }
        }
    }
    hits
}

/// The model's edge against the picture's, in mm, at nine stations along one side; positive
/// where the model reaches past the picture, null where a station misses one of the two.
fn edge_offsets(model: &Placed, picture: &Part, side: Edge, mmpp: f64) -> Vec<Value> {
    (1..=STATIONS)
        .map(|i| {
            let station = match side {
                Edge::Top | Edge::Bottom => (picture.width as u64 * i as u64 / (STATIONS as u64 + 1)) as usize,
                Edge::Left | Edge::Right => (picture.height as u64 * i as u64 / (STATIONS as u64 + 1)) as usize,
            };
            let offset = match side {
                Edge::Top | Edge::Bottom => {
                    let pic = column_edge(&picture.mask, station, matches!(side, Edge::Top));
                    let mdl = (station as i64 - model.dx)
                        .try_into()
                        .ok()
                        .and_then(|mx: usize| column_edge(model.mask, mx, matches!(side, Edge::Top)))
                        .map(|e| e as i64 + model.dy);
                    match (mdl, pic) {
                        (Some(m), Some(p)) => Some(match side {
                            Edge::Top => (p as f64 - m as f64) * mmpp,
                            _ => (m as f64 - p as f64) * mmpp,
                        }),
                        _ => None,
                    }
                }
                Edge::Left | Edge::Right => {
                    let pic = row_edge(&picture.mask, station, matches!(side, Edge::Left));
                    let mdl = (station as i64 - model.dy)
                        .try_into()
                        .ok()
                        .and_then(|my: usize| row_edge(model.mask, my, matches!(side, Edge::Left)))
                        .map(|e| e as i64 + model.dx);
                    match (mdl, pic) {
                        (Some(m), Some(p)) => Some(match side {
                            Edge::Left => (p as f64 - m as f64) * mmpp,
                            _ => (m as f64 - p as f64) * mmpp,
                        }),
                        _ => None,
                    }
                }
            };
            offset.map(|mm| json!(round3(mm))).unwrap_or(Value::Null)
        })
        .collect()
}

/// The first (or last, when `first` is false) row holding column `x`; None if none does.
fn column_edge(mask: &[Vec<bool>], x: usize, first: bool) -> Option<usize> {
    let mut found = None;
    for (y, row) in mask.iter().enumerate() {
        if row.get(x).copied().unwrap_or(false) {
            if first {
                return Some(y);
            }
            found = Some(y);
        }
    }
    found
}

/// The first (or last) column set in row `y`; None if the row is empty or missing.
fn row_edge(mask: &[Vec<bool>], y: usize, first: bool) -> Option<usize> {
    let row = mask.get(y)?;
    if first { row.iter().position(|&on| on) } else { row.iter().rposition(|&on| on) }
}

/// The overlay PNG: the picture in blue, the model in red, on a canvas that holds both.
fn overlay_png(picture: &Part, model: &Placed) -> Vec<u8> {
    // A margin so a model that hangs off the picture still fits the canvas.
    let m = 8i64;
    let (x0, y0) = (0.min(model.dx) - m, 0.min(model.dy) - m);
    let (x1, y1) = (
        (picture.width as i64).max(model.dx + model.mask.first().map_or(0, |r| r.len()) as i64) + m,
        (picture.height as i64).max(model.dy + model.mask.len() as i64) + m,
    );
    let (w, h) = ((x1 - x0).max(1) as u32, (y1 - y0).max(1) as u32);
    let mut img = image::RgbaImage::from_pixel(w, h, image::Rgba([250, 250, 250, 255]));
    for (y, row) in picture.mask.iter().enumerate() {
        for (x, &on) in row.iter().enumerate() {
            if on {
                img.put_pixel((x as i64 - x0) as u32, (y as i64 - y0) as u32, image::Rgba([70, 110, 230, 255]));
            }
        }
    }
    for (r, row) in model.mask.iter().enumerate() {
        for (c, &on) in row.iter().enumerate() {
            if !on {
                continue;
            }
            let (x, y) = (model.dx + c as i64 - x0, model.dy + r as i64 - y0);
            if (0..w as i64).contains(&x) && (0..h as i64).contains(&y) {
                img.put_pixel(x as u32, y as u32, image::Rgba([235, 60, 60, 255]));
            }
        }
    }
    let mut out = Cursor::new(Vec::new());
    // Encoding a mask-sized PNG cannot realistically fail; if it ever does, no overlay.
    let enc = image::codecs::png::PngEncoder::new(&mut out);
    use image::ImageEncoder as _;
    if enc.write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8).is_err() {
        return Vec::new();
    }
    out.into_inner()
}

fn round4(v: f64) -> f64 {
    if v.abs() < 1e12 { (v * 10_000.0).round() / 10_000.0 } else { v }
}

fn round3(v: f64) -> f64 {
    if v.abs() < 1e12 { (v * 1_000.0).round() / 1_000.0 } else { v }
}
