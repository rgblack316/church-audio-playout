//! Usage: cargo run -p playout-audio --example probe -- <audio file>
//! Decodes a file and prints what the engine would see.
fn main() {
    let path = std::env::args().nth(1).expect("usage: probe <file>");
    match playout_audio::decode_file(std::path::Path::new(&path)) {
        Ok(d) => println!(
            "OK  {} ch, {} Hz, {:.2} s, {:.1} MB in memory",
            d.channels,
            d.sample_rate,
            d.duration_secs(),
            (d.samples.len() * 2) as f64 / 1_048_576.0
        ),
        Err(e) => println!("FAILED: {e}"),
    }
}
