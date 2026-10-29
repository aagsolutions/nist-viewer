//! Dumps all decodable images from a NIST file to /tmp as PNGs.
fn main() {
    let path = std::env::args().nth(1).expect("usage: debug_decode <file.an2|file.wsq>");
    let bytes = std::fs::read(&path).unwrap();
    if path.ends_with(".wsq") {
        let d = nist_viewer::wsq::decode(&bytes).unwrap();
        println!("WSQ {}x{} ppi={:?}", d.width, d.height, d.ppi);
        image::GrayImage::from_raw(d.width, d.height, d.pixels).unwrap()
            .save("/tmp/wsq_standalone.png").unwrap();
        return;
    }
    let nist = nist_viewer::nist::parser::decode(&bytes).unwrap();
    for (i, rec) in nist.all_records().enumerate() {
        let Some(data) = &rec.image_data else { continue };
        let comp = rec.compression_algorithm().unwrap_or("NONE");
        println!("record {i}: type {} comp={comp} {} bytes, starts {:02x} {:02x}",
                 rec.record_type, data.len(), data[0], data[1]);
        if data.starts_with(&[0xFF, 0xA0]) {
            match nist_viewer::wsq::decode(data) {
                Ok(d) => {
                    println!("  WSQ decoded {}x{} ppi={:?} min={} max={}",
                             d.width, d.height, d.ppi,
                             d.pixels.iter().min().unwrap(), d.pixels.iter().max().unwrap());
                    let img = image::GrayImage::from_raw(d.width, d.height, d.pixels).unwrap();
                    let out = format!("/tmp/wsq_dump_{i}.png");
                    img.save(&out).unwrap();
                    println!("  wrote {out}");
                }
                Err(e) => println!("  WSQ decode FAILED: {e:#}"),
            }
        }
    }
}
