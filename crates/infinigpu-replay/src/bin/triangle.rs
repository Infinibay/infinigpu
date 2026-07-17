//! Phase-0 Step-5 shader + dma-buf proof: open the physical GPU, draw a **shader-executed**
//! triangle (real SM execution, per-vertex colour interpolation — not a fixed-function
//! clear), verify the read-back pixels, then **export** the rendered GPU memory as a
//! dma-buf/opaque fd (zero-copy hand-off) and check the fd + size.
//!
//! ```sh
//! cargo run -p infinigpu-replay --bin infinigpu-replay-triangle [-- out.ppm]
//! ```

use infinigpu_replay::HostGpu;
use std::collections::HashSet;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/infinigpu-replay-triangle.ppm".to_string());

    let gpu = HostGpu::open()?;
    println!("infinigpu-replay triangle + dma-buf test");
    println!("  GPU:     {}", gpu.device_name());
    println!("  driver:  {} ({:?})", gpu.driver_name(), gpu.driver_id());
    println!("  export:  {}", if gpu.can_export() { "supported" } else { "unavailable" });

    const W: u32 = 512;
    const H: u32 = 512;
    let bg = [0.05_f32, 0.05, 0.08, 1.0]; // dark background → ~[13,13,20]

    // ---- render the shader triangle ----
    let t = Instant::now();
    let frame = gpu.render_triangle(W, H, bg)?;
    println!("  render:  {W}x{H} triangle in {:.2} ms", t.elapsed().as_secs_f64() * 1e3);

    // Center is inside the triangle (centroid ≈ NDC (0, 0.2)); corners are background.
    let center = frame.pixel(W / 2, H / 2);
    let c00 = frame.pixel(2, 2);
    let c11 = frame.pixel(W - 3, H - 3);
    let bright = |p: [u8; 4]| p[0].max(p[1]).max(p[2]) > 100;
    let dark = |p: [u8; 4]| p[0] < 60 && p[1] < 60 && p[2] < 60;
    println!("  center({},{}) = {:?}  (triangle → bright)", W / 2, H / 2, center);
    println!("  corner(2,2)      = {c00:?}  (background → dark)");
    println!("  corner({},{}) = {c11:?}  (background → dark)", W - 3, H - 3);

    // A clear yields ~1 colour; an interpolated gradient yields many. Sample a grid.
    let mut hues: HashSet<[u8; 4]> = HashSet::new();
    for gy in 0..32 {
        for gx in 0..32 {
            hues.insert(frame.pixel(gx * W / 32, gy * H / 32));
        }
    }
    println!("  distinct colours in 32×32 sample: {}", hues.len());

    let mut bad = 0;
    if !bright(center) {
        eprintln!("  !! center is not bright — triangle did not render");
        bad += 1;
    }
    if !dark(c00) || !dark(c11) {
        eprintln!("  !! a corner is not background");
        bad += 1;
    }
    if hues.len() < 20 {
        eprintln!("  !! too few distinct colours ({}) — no shader interpolation", hues.len());
        bad += 1;
    }
    if bad != 0 {
        return Err(format!("{bad} triangle check(s) failed").into());
    }
    std::fs::write(&out_path, frame.to_ppm())?;
    println!("  saved:   {}", std::fs::canonicalize(&out_path).unwrap_or_else(|_| out_path.clone().into()).display());

    // ---- export the rendered GPU memory as an fd ----
    if gpu.can_export() {
        let (_frame2, export) = gpu.export_triangle_dmabuf(W, H)?;
        let want = (W * H * 4) as u64;
        println!(
            "  export:  fd={} type={} size={} bytes (>= {want})",
            export.raw_fd(),
            export.handle_type(),
            export.size()
        );
        if export.raw_fd() < 0 {
            return Err("exported fd is invalid".into());
        }
        if export.size() < want {
            return Err(format!("exported size {} < frame {}", export.size(), want).into());
        }
        // Dropping `export` closes the fd.
        println!(
            "\nOK — GPU executed our shaders (gradient triangle) and exported the result as a {} fd.",
            export.handle_type()
        );
    } else {
        println!("\nOK — GPU executed our shaders (gradient triangle). (fd export unavailable on this driver.)");
    }
    Ok(())
}
