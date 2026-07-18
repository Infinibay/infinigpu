//! Headless probe: connect, decode N frames, verify them, and optionally save the last
//! as a PPM — no window. This is the CI/dev path (works on a box with no display) and
//! proves the whole net → protocol → decode pipeline against a running server.

use crate::stream::{run_stream, CursorSlot, DecodedFrame};
use std::sync::Arc;
use std::error::Error;
use std::fs::File;
use std::io::Write;

pub fn run(url: &str, frames: usize, out: Option<&str>) -> Result<(), Box<dyn Error>> {
    let mut got = 0usize;
    let mut keyframes = 0usize;
    let mut last: Option<DecodedFrame> = None;

    run_stream(url, None::<std::sync::mpsc::Receiver<String>>, None::<Arc<CursorSlot>>, |f| {
        got += 1;
        if f.keyframe {
            keyframes += 1;
        }
        // Count non-black pixels so we can assert real content flowed, not a blank frame.
        let nonblank = f
            .rgba
            .chunks_exact(4)
            .filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8)
            .count();
        println!(
            "  frame seq={:<4} {}x{} {:>7} non-black px {}",
            f.seq,
            f.width,
            f.height,
            nonblank,
            if f.keyframe { "[KEY]" } else { "" }
        );
        last = Some(f);
        got < frames
    })?;

    let last = last.ok_or("no frames decoded")?;
    println!("\nDecoded {got} frame(s), {keyframes} keyframe(s); last = {}x{}", last.width, last.height);

    if let Some(path) = out {
        write_ppm(path, &last)?;
        println!("Saved last frame: {path}");
    }
    if got < frames {
        return Err(format!("stream ended after {got}/{frames} frames").into());
    }
    let nonblank = last
        .rgba
        .chunks_exact(4)
        .filter(|p| p[0] > 8 || p[1] > 8 || p[2] > 8)
        .count();
    if nonblank == 0 {
        return Err("last frame is entirely black — decode produced no content".into());
    }
    println!("OK — infiniPixel stream decoded to real RGBA frames.");
    Ok(())
}

fn write_ppm(path: &str, f: &DecodedFrame) -> Result<(), Box<dyn Error>> {
    let mut file = File::create(path)?;
    write!(file, "P6\n{} {}\n255\n", f.width, f.height)?;
    // RGBA → RGB (drop alpha).
    let mut rgb = Vec::with_capacity((f.width * f.height * 3) as usize);
    for px in f.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&px[0..3]);
    }
    file.write_all(&rgb)?;
    Ok(())
}
