//! A command-line tool with Zharp: `ZHARP_KEY=zpk_… cargo run --example cli --features anyhow -- <file>`
//!
//! A missing file is reported (with the stack of where the anyhow error was created); `panic` as
//! the argument panics in `main`, which is reported before the program ends.
use anyhow::Context as _;

fn load(path: &str) -> anyhow::Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("could not load {path}"))
}

fn main() -> anyhow::Result<()> {
    let _zharp = zharp::init!();
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "orders.csv".into());
    if path == "panic" {
        let empty: Vec<u32> = Vec::new();
        println!("{}", empty[0]);
    }
    match load(&path) {
        Ok(text) => println!("{} bytes", text.len()),
        Err(error) => {
            zharp::capture_anyhow(&error);
            tracing::error!(error = ?error, "giving up");
        }
    }
    Ok(())
}
