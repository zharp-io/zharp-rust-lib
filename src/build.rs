//! The `build.rs` helper: an ID for this build of your code, so an error's frames show the code
//! that ran from the source bundle `npx @zharp/cli sources upload` sends.
//!
//! ```toml
//! [build-dependencies]
//! zharp = { version = "0.1", default-features = false }
//! ```
//!
//! ```no_run
//! // build.rs
//! fn main() {
//!     zharp::build::id();
//! }
//! ```
//!
//! The ID is a hash of the crate's `src/**/*.rs` files (their paths and contents), so the CLI,
//! hashing the same files, uploads the bundle under the same ID without being told it.
//! [`init!`](crate::init) puts it into the binary and the SDK sends it with each error as
//! `zharp.build_id`. The git commit, when there is one, becomes part of the default release.
//!
//! Without the `sdk` feature (`default-features = false`) this module is all that is compiled:
//! nothing of OpenTelemetry is built twice.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Writes the build ID (and the git commit) for [`init!`](crate::init) to read. Call it from
/// `build.rs`.
pub fn id() {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_else(|| ".".into()));
    let id = source_id(&root);
    println!("cargo:rustc-env=ZHARP_BUILD_ID={id}");
    println!("cargo:rerun-if-changed={}", root.join("src").display());
    if let Some(sha) = git_sha(&root) {
        println!("cargo:rustc-env=ZHARP_GIT_SHA={sha}");
        if let Some(head) = git_path(&root, "logs/HEAD").or_else(|| git_path(&root, "HEAD")) {
            println!("cargo:rerun-if-changed={}", head.display());
        }
    }
}

/// The build ID of the crate at `root`: the first 32 hex digits of a SHA-256 over its
/// `src/**/*.rs` files, sorted by path, each as `path\0contents\0` (paths from `root`, forward
/// slashes). `npx @zharp/cli sources upload` computes the same.
pub fn source_id(root: &Path) -> String {
    let mut files = Vec::new();
    collect(&root.join("src"), "src", &mut files);
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut hasher = Sha256::new();
    for (path, full) in files {
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        hasher.update(path.as_bytes());
        hasher.update(&[0]);
        hasher.update(&bytes);
        hasher.update(&[0]);
    }
    hasher.finish()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn collect(dir: &Path, relative: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let rel = format!("{relative}/{name}");
        match entry.file_type() {
            Ok(t) if t.is_dir() => collect(&path, &rel, out),
            Ok(t) if t.is_file() && name.ends_with(".rs") => out.push((rel, path)),
            _ => {}
        }
    }
}

fn git_sha(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !sha.is_empty()).then_some(sha)
}

fn git_path(root: &Path, name: &str) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-path", name])
        .current_dir(root)
        .output()
        .ok()?;
    let path = root.join(String::from_utf8(out.stdout).ok()?.trim());
    (out.status.success() && path.exists()).then_some(path)
}

/// SHA-256 (FIPS 180-4), so the helper needs no dependencies.
struct Sha256 {
    state: [u32; 8],
    buffer: Vec<u8>,
    length: u64,
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

impl Sha256 {
    fn new() -> Self {
        Sha256 {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buffer: Vec::with_capacity(64),
            length: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.length += data.len() as u64;
        while !data.is_empty() {
            let take = (64 - self.buffer.len()).min(data.len());
            self.buffer.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.buffer.len() == 64 {
                let block: [u8; 64] = self.buffer[..].try_into().unwrap_or([0; 64]);
                self.compress(&block);
                self.buffer.clear();
            }
        }
    }

    fn finish(mut self) -> [u8; 32] {
        let bits = self.length.wrapping_mul(8);
        let mut tail = vec![0x80u8];
        while (self.buffer.len() + tail.len()) % 64 != 56 {
            tail.push(0);
        }
        tail.extend_from_slice(&bits.to_be_bytes());
        let length = self.length;
        self.update(&tail);
        self.length = length;
        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *s = s.wrapping_add(v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_matches_known_digests() {
        assert_eq!(
            hex(&Sha256::new().finish()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let mut h = Sha256::new();
        h.update(b"abc");
        assert_eq!(
            hex(&h.finish()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut h = Sha256::new();
        for _ in 0..1000 {
            h.update(b"abcdefghij"); // crosses block boundaries unevenly
        }
        let mut once = Sha256::new();
        once.update(&b"abcdefghij".repeat(1000));
        assert_eq!(hex(&h.finish()), hex(&once.finish()));
        let mut h = Sha256::new();
        h.update(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq");
        assert_eq!(
            hex(&h.finish()),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn source_id_hashes_paths_and_contents() {
        let dir = std::env::temp_dir().join(format!("zharp-build-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src/cart")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/cart/mod.rs"), "pub fn total() {}\n").unwrap();
        std::fs::write(dir.join("src/notes.txt"), "not code").unwrap();
        let id = source_id(&dir);
        assert_eq!(id.len(), 32);
        // The same as hashing "src/cart/mod.rs\0…\0src/main.rs\0…\0" (the CLI's recipe).
        let mut h = Sha256::new();
        h.update(b"src/cart/mod.rs\0pub fn total() {}\n\0src/main.rs\0fn main() {}\n\0");
        assert_eq!(id, hex(&h.finish()[..16]));
        std::fs::write(dir.join("src/main.rs"), "fn main() { }\n").unwrap();
        assert_ne!(source_id(&dir), id);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
