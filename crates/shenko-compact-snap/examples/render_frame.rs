//! Render stdin with the snapcompact frame renderer, writing frame-N.png into
//! the given directory. For eyeballing legibility and feeding vision-accuracy runs.
use std::io::Read;

fn main() -> std::io::Result<()> {
    let out = std::env::args().nth(1).expect("usage: render_frame <out-dir> < text");
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let rendered = shenko_compact_snap::render::render(&text, 160, 64);
    for (i, frame) in rendered.pngs.iter().enumerate() {
        std::fs::write(format!("{out}/frame-{i}.png"), &frame.png)?;
        println!("frame-{i}.png {}x{} {} bytes", frame.width, frame.height, frame.png.len());
    }
    println!("unrenderable: {}", rendered.unrenderable);
    Ok(())
}
