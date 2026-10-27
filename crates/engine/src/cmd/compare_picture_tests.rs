//! Tests for `inspect.compare_picture` (#1486): synthetic reference pictures drawn in code,
//! written to the temp dir and read back through the vfs like any other reference.

use std::io::Cursor;

use serde_json::{Value, json};

use super::super::Session;
use image::ImageEncoder as _;

fn run(s: &mut Session, cmd: &str, params: Value) -> Value {
    s.execute(cmd, &params).unwrap_or_else(|e| panic!("{cmd}: {e}"))
}

fn box_body(s: &mut Session, name: &str, x1: f64, y1: f64, z: f64) {
    let sketch = format!("{name}Sketch");
    run(s, "sketch.create", json!({"plane": "XY", "name": sketch}));
    run(s, "sketch.rectangle.two_point", json!({"p0": [0.0, 0.0], "p1": [x1, y1]}));
    run(s, "sketch.finish", json!({}));
    run(s, "solid.extrude", json!({"sketch": sketch, "distance": z, "operation": "new_body", "body_name": name}));
}

/// A PNG whose pixels come from `paint`.
fn png(w: usize, h: usize, paint: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
    let mut img = image::RgbaImage::new(w as u32, h as u32);
    for y in 0..h {
        for x in 0..w {
            let [r, g, b] = paint(x, y);
            img.put_pixel(x as u32, y as u32, image::Rgba([r, g, b, 255]));
        }
    }
    let mut out = Cursor::new(Vec::new());
    image::codecs::png::PngEncoder::new(&mut out).write_image(img.as_raw(), w as u32, h as u32, image::ExtendedColorType::Rgba8).unwrap();
    out.into_inner()
}

fn jpeg(w: usize, h: usize, paint: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
    let mut img = image::RgbaImage::new(w as u32, h as u32);
    for y in 0..h {
        for x in 0..w {
            let [r, g, b] = paint(x, y);
            img.put_pixel(x as u32, y as u32, image::Rgba([r, g, b, 255]));
        }
    }
    let rgb = image::DynamicImage::ImageRgba8(img).to_rgb8();
    let mut out = Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .write_image(rgb.as_raw(), w as u32, h as u32, image::ExtendedColorType::Rgb8)
        .unwrap();
    out.into_inner()
}

/// Write a reference into the temp dir and return its path.
fn reference(name: &str, bytes: &[u8]) -> String {
    let path = std::env::temp_dir().join(format!("solidcraft-compare-picture-{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path.to_string_lossy().into_owned()
}

/// A white rectangle on a grey backdrop: `x0..x1, y0..y1` in pixels.
fn rect_picture(w: usize, h: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> Vec<u8> {
    png(w, h, |x, y| if (x0..x1).contains(&x) && (y0..y1).contains(&y) { [255, 255, 255] } else { [200, 200, 200] })
}

fn edges(v: &Value) -> Vec<Vec<Option<f64>>> {
    ["top", "bottom", "left", "right"]
        .iter()
        .map(|side| v["edges"][side].as_array().unwrap().iter().map(|e| e.as_f64()).collect::<Vec<Option<f64>>>())
        .collect()
}

fn near(v: Option<f64>, want: f64, tol: f64) -> bool {
    v.is_some_and(|e| (e - want).abs() <= tol)
}

#[test]
fn a_matching_picture_scores_high_with_flat_edges() {
    let mut s = Session::default();
    box_body(&mut s, "Box", 20.0, 10.0, 30.0); // front view: 20 wide, 30 tall
    let path = reference("match.png", &rect_picture(200, 300, 20, 20, 180, 280));
    let out = run(&mut s, "inspect.compare_picture", json!({"reference": path, "view": "front"}));
    assert!(out["overlap"].as_f64().unwrap() > 0.95, "overlap {}", out["overlap"]);
    for side in edges(&out) {
        for e in side {
            assert!(near(e, 0.0, 0.6), "edge {e:?}");
        }
    }
    assert!(!out["overlay_png"].as_str().unwrap().is_empty());
}

#[test]
fn a_step_in_the_bottom_edge_shows_up_in_mm() {
    let mut s = Session::default();
    box_body(&mut s, "Box", 20.0, 10.0, 30.0);
    // The aligned picture: a plain part 160 x 260 px.
    let aligned_path = reference("aligned.png", &rect_picture(200, 300, 20, 20, 180, 280));
    let aligned = run(&mut s, "inspect.compare_picture", json!({"reference": aligned_path, "view": "front"}));
    // The stepped picture: over its right half the part's bottom edge sits 35 px higher.
    // fit=both scales the model's 30 mm to the part's 260 px, so 35 px is ~4.04 mm of the
    // model reaching past the picture.
    let stepped_path = reference(
        "stepped.png",
        &png(200, 300, |x, y| {
            let bottom = if x >= 100 { 245 } else { 280 };
            if (20..180).contains(&x) && (20..bottom).contains(&y) { [255, 255, 255] } else { [200, 200, 200] }
        }),
    );
    let stepped = run(&mut s, "inspect.compare_picture", json!({"reference": stepped_path, "view": "front"}));
    let flat = edges(&aligned)[1].clone();
    let step = edges(&stepped)[1].clone();
    for e in flat.iter() {
        assert!(near(*e, 0.0, 0.6), "flat bottom {e:?}");
    }
    // Stations sit at local x = 16..144 px; the notch starts at local x = 80 (global 100),
    // so stations 5..9 are over it.
    for e in step.iter().take(4) {
        assert!(near(*e, 0.0, 0.6), "step bottom left {e:?}");
    }
    for e in step.iter().skip(4) {
        assert!(near(*e, 4.04, 0.6), "step bottom right {e:?}");
    }
    assert!(stepped["overlap"].as_f64().unwrap() < aligned["overlap"].as_f64().unwrap());
}

#[test]
fn a_gradient_backdrop_still_separates_from_the_part() {
    let mut s = Session::default();
    box_body(&mut s, "Box", 20.0, 10.0, 30.0);
    // A backdrop that shifts one step per row: the border flood follows it; the step at the
    // part's outline (to white) stops it.
    let path = reference(
        "gradient.png",
        &png(200, 300, |x, y| if (20..180).contains(&x) && (20..280).contains(&y) { [255, 255, 255] } else { [200 + (y / 30) as u8, 200, 200] }),
    );
    let out = run(&mut s, "inspect.compare_picture", json!({"reference": path, "view": "front"}));
    assert!(out["overlap"].as_f64().unwrap() > 0.9, "overlap {}", out["overlap"]);
}

#[test]
fn a_jpeg_reference_works_too() {
    let mut s = Session::default();
    box_body(&mut s, "Box", 20.0, 10.0, 30.0);
    let path =
        reference("match.jpg", &jpeg(200, 300, |x, y| if (20..180).contains(&x) && (20..280).contains(&y) { [255, 255, 255] } else { [40, 40, 40] }));
    let out = run(&mut s, "inspect.compare_picture", json!({"reference": path, "view": "front"}));
    assert!(out["overlap"].as_f64().unwrap() > 0.9, "overlap {}", out["overlap"]);
}

#[test]
fn crop_takes_one_part_off_the_board() {
    let mut s = Session::default();
    box_body(&mut s, "Box", 20.0, 10.0, 30.0);
    // Two parts side by side; the crop keeps the left one, a plain rectangle that matches
    // the model. The right part is bigger overall but L-shaped, so without the crop the
    // largest piece wins and the score drops.
    let path = reference(
        "two.png",
        &png(400, 300, |x, y| {
            let left = (20..180).contains(&x) && (20..280).contains(&y);
            let right = (210..390).contains(&x) && (5..295).contains(&y) && !((300..390).contains(&x) && (100..200).contains(&y));
            if left || right { [255, 255, 255] } else { [200, 200, 200] }
        }),
    );
    let out = run(&mut s, "inspect.compare_picture", json!({"reference": path.clone(), "view": "front", "crop": [0.0, 0.0, 0.5, 1.0]}));
    // The picture size is the found part's tight frame: the left rectangle is 160 x 260 px.
    assert_eq!(out["picture"]["width"], 160);
    assert!(out["overlap"].as_f64().unwrap() > 0.95, "overlap {}", out["overlap"]);
    let whole = run(&mut s, "inspect.compare_picture", json!({"reference": path, "view": "front"}));
    assert!(whole["overlap"].as_f64().unwrap() < out["overlap"].as_f64().unwrap());
}

#[test]
fn fit_width_keeps_the_models_aspect() {
    let mut s = Session::default();
    box_body(&mut s, "Box", 20.0, 10.0, 30.0);
    // The picture part is 160 x 160 px: fit=width scales the model uniformly to 160 x 240,
    // so it hangs 80 px (10 mm) over the picture's height in total, wherever the slide puts
    // it. The overlap of a contained model is 160/240 wherever it sits.
    let path = reference("short.png", &rect_picture(200, 220, 20, 20, 180, 180));
    let out = run(&mut s, "inspect.compare_picture", json!({"reference": path, "view": "front", "fit": "width"}));
    assert!((out["overlap"].as_f64().unwrap() - 160.0 / 240.0).abs() < 0.05, "overlap {}", out["overlap"]);
    let bottom: Vec<Option<f64>> = out["edges"]["bottom"].as_array().unwrap().iter().map(|e| e.as_f64()).collect();
    let top: Vec<Option<f64>> = out["edges"]["top"].as_array().unwrap().iter().map(|e| e.as_f64()).collect();
    for i in 0..9 {
        // The overhang splits between top and bottom but always totals the extra 10 mm.
        assert!(bottom[i].unwrap() >= 0.0 && top[i].unwrap() >= 0.0, "bottom {:?} top {:?}", bottom[i], top[i]);
        assert!((bottom[i].unwrap() + top[i].unwrap() - 10.0).abs() < 0.6, "bottom {:?} top {:?}", bottom[i], top[i]);
    }
}

#[test]
fn hostile_and_empty_references_fail_cleanly() {
    let mut s = Session::default();
    box_body(&mut s, "Box", 20.0, 10.0, 30.0);
    assert!(s.execute("inspect.compare_picture", &json!({})).is_err());
    // Not an image at all.
    let text = reference("not.png", b"just text, not a picture");
    assert!(s.execute("inspect.compare_picture", &json!({"reference": text})).unwrap_err().to_string().contains("PNG or a JPEG"));
    // A uniform picture: the background eats everything.
    let flat = reference("flat.png", &png(64, 64, |_, _| [200, 200, 200]));
    assert!(s.execute("inspect.compare_picture", &json!({"reference": flat})).unwrap_err().to_string().contains("no part found"));
    // A missing file.
    assert!(s.execute("inspect.compare_picture", &json!({"reference": "no-such-file.png"})).is_err());
    // A bad fit name.
    let path = reference("match2.png", &rect_picture(200, 300, 20, 20, 180, 280));
    assert!(s.execute("inspect.compare_picture", &json!({"reference": path, "fit": "diagonal"})).is_err());
    // overlay: false leaves the PNG out.
    let out = run(&mut s, "inspect.compare_picture", json!({"reference": path, "view": "front", "overlay": false}));
    assert!(out.get("overlay_png").is_none());
}
