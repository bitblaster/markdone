//! Headless typing benchmark: open a file, put the caret mid-document, type N chars and
//! time each keystroke through the editor core (no window, no layout).
use std::time::Instant;
use writ::core::Editor;

fn main() {
    let path = std::env::args().nth(1).expect("usage: typebench <file.md> [n]");
    let n: usize = std::env::args().nth(2).map_or(20, |s| s.parse().unwrap());
    let t = Instant::now();
    let mut ed = Editor::open(std::path::Path::new(&path));
    println!("open: {:.1} ms, {} bytes", t.elapsed().as_secs_f64() * 1e3, ed.len());
    ed.set_cursor(ed.len() / 2);
    let lines = ed.line_count();
    for i in 0..n {
        let c = (b'a' + (i % 26) as u8) as char;
        let t = Instant::now();
        ed.type_char(c);
        let t_type = t.elapsed();
        let t = Instant::now();
        ed.refresh_detection(0..lines);
        let t_det = t.elapsed();
        println!(
            "key {i:2}: type_char {:7.2} ms | refresh_detection {:7.2} ms",
            t_type.as_secs_f64() * 1e3,
            t_det.as_secs_f64() * 1e3
        );
    }
}
