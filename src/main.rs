static WORKER_JS: &[u8] = include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/worker.js"));

fn main() {
    println!("Hello, world!");
}
