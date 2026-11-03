//! Write the synthetic fixture EPUB and matching source cache for QA.
//!
//! Usage:
//!     cargo run --example write_fixture -- <fixture-dir>
//!
//! The generated `fixture.epub` and `book.json` can be passed to the
//! developer-only Python fast scorer in the royal-road-epub checkout.

#[path = "../tests/fixture/mod.rs"]
#[allow(dead_code)]
mod fixture;

use std::path::PathBuf;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let out_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("qa/fixture"));
    std::fs::create_dir_all(&out_dir)?;

    let book = fixture::synthetic_book();
    let epub_path = out_dir.join("fixture.epub");
    let source_cache_path = out_dir.join("book.json");

    let _cache_guard = fixture::seed_inline_image_cache(&book)?;
    rr_to_epub::epub::write_epub(&book, Some(epub_path.to_string_lossy().into_owned()), false)
        .await?;
    fixture::write_source_cache(&source_cache_path, &book)?;

    println!("Wrote {}", epub_path.display());
    println!("Wrote {}", source_cache_path.display());
    Ok(())
}
