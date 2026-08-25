//! Build one named fixture shape into a directory.
//!
//! The seam that lets `ci/browser/fixture.mjs` use the same builders the Rust
//! suites use, instead of reimplementing them in JavaScript (ADR 0076):
//!
//! ```text
//! gv-fixture <shape> <directory>
//! ```
//!
//! The directory is emptied first, so a run always produces the shape rather
//! than the shape merged with whatever was there before. Exits non-zero, with
//! the list of valid names on stderr, if the shape is not one it knows.

use git_vista_fixtures::browser::SHAPES;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [shape, dir] = args.as_slice() else {
        eprintln!("usage: gv-fixture <shape> <directory>");
        eprintln!("shapes: {}", names());
        std::process::exit(2);
    };

    let Some((_, build)) = SHAPES.iter().find(|(name, _)| name == shape) else {
        eprintln!("gv-fixture: unknown shape {shape:?}");
        eprintln!("shapes: {}", names());
        std::process::exit(2);
    };

    build(std::path::Path::new(dir));
}

fn names() -> String {
    SHAPES
        .iter()
        .map(|(n, _)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}
