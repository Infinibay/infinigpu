//! Phase-0 Step-5 smoke test: open the physical GPU headless, render one frame,
//! read it back, verify the pixels, and save a PPM. This is the "does the GPU half
//! of the loop actually run on our hardware?" check — no QEMU, no guest.
//!
//! ```sh
//! cargo run -p infinigpu-replay --bin infinigpu-replay-smoke [-- out.ppm]
//! ```

use infinigpu_replay::HostGpu;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "target/infinigpu-replay-clear.ppm".to_string());

    let t0 = Instant::now();
    let gpu = HostGpu::open()?;
    let open_ms = t0.elapsed().as_secs_f64() * 1e3;

    println!("infinigpu-replay smoke test");
    println!("  GPU:     {}", gpu.device_name());
    println!("  driver:  {} ({:?})", gpu.driver_name(), gpu.driver_id());
    println!("  open:    {open_ms:.1} ms");

    // Clear values chosen to map to exact 8-bit values (no rounding ambiguity):
    // 0.0->0, 0.6->153, 0.8->204, 1.0->255. A recognisable infinigpu teal.
    const W: u32 = 512;
    const H: u32 = 512;
    let clear = [0.0_f32, 0.6, 0.8, 1.0];
    let expected = [0u8, 153, 204, 255];

    let t1 = Instant::now();
    let frame = gpu.render_clear(W, H, clear)?;
    let render_ms = t1.elapsed().as_secs_f64() * 1e3;
    println!("  render:  {W}x{H} in {render_ms:.2} ms");

    // Verify a few pixels came back exactly as cleared (proves execute+readback).
    let mut bad = 0usize;
    for &(x, y) in &[(0, 0), (W / 2, H / 2), (W - 1, H - 1), (7, 300)] {
        let p = frame.pixel(x, y);
        let ok = (0..4).all(|c| (p[c] as i32 - expected[c] as i32).abs() <= 1);
        println!(
            "  pixel({x:>3},{y:>3}) = {:?}  expected {:?}  {}",
            p,
            expected,
            if ok { "OK" } else { "MISMATCH" }
        );
        if !ok {
            bad += 1;
        }
    }
    if bad != 0 {
        return Err(format!("{bad} pixel(s) did not match the cleared colour").into());
    }

    std::fs::write(&out_path, frame.to_ppm())?;
    let abs = std::fs::canonicalize(&out_path).unwrap_or_else(|_| out_path.clone().into());
    println!("  saved:   {}", abs.display());
    println!("\nOK — the physical GPU rendered and returned the frame; datapath verified.");
    Ok(())
}
