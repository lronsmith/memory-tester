//! Entry point. The GUI is added by a later feature; until then this only
//! proves the crate builds into an executable.

fn main() {
    println!("memory-tester {}", env!("CARGO_PKG_VERSION"));
}
