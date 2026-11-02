//! Shared synthetic book fixture for the EPUB writer tests and the QA harness.
//!
//! The fixture is deliberately raw: it keeps the source notice, spacer
//! paragraph, unused classes, inline styles, and author-note promos that later
//! tickets will clean. It never needs the network because the cover travels in
//! `Book::cover` and both inline images are seeded into the inline-image cache.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use rr_to_epub::cache::Cache;
use rr_to_epub::epub::{Book, Chapter};

pub const BOOK_ID: u32 = 2_000_000_002;
pub const BOOK_TITLE: &str = "Synthetic Clean EPUB Fixture";
pub const BOOK_AUTHOR: &str = "Fixture Author";
pub const BOOK_DESCRIPTION: &str = "A synthetic book used by the EPUB writer tests.";
pub const BOOK_DATE: &str = "2024-01-01";

pub const CHAPTER_ONE_TITLE: &str = "Chapter 1: The Beginning";
pub const ANNOUNCEMENT_TITLE: &str = "Announcement: Schedule Change";
pub const CHAPTER_TWO_TITLE: &str = "Chapter 2: The Author's Note";

pub const PROSE_LINK: &str = "https://example.com/story-reference";
pub const PROMO_LINK: &str = "https://example.com/promo";
pub const PROMO_IMAGE_URL: &str = "https://example.com/assets/promo.png";
pub const PROMO_IMAGE_FILENAME: &str = "promo.png";
pub const STORY_IMAGE_URL: &str = "https://example.com/assets/story-map.png?v=1";
pub const STORY_IMAGE_FILENAME: &str = "story-map.png";
pub const COVER_URL: &str = "https://example.com/assets/cover.jpg";

pub const NOTICE_TEXT: &str = "Copied from Royal Road. This chapter was reproduced without the author's consent. Please support the official release and the original work.";
pub const NOTE_TEXT: &str =
    "Thanks for reading. The next arc starts next week and I hope you enjoy it.";
/// Long enough, and with two notice keywords, to prove the source-notice rule
/// never runs inside an author-note container.
pub const NOTE_NOTICE_TEXT: &str = "This author note was copied from my private notebook with the author's permission, and the chapter it mentions will continue next week as planned.";
/// Contains one notice keyword only; the notice rule must leave it alone.
pub const FALSE_NOTICE_TEXT: &str = "This copied passage is a short example of prose that should remain in the story even though it mentions one notice-like word.";

pub fn expected_chapter_titles() -> [&'static str; 3] {
    [CHAPTER_ONE_TITLE, ANNOUNCEMENT_TITLE, CHAPTER_TWO_TITLE]
}

pub fn synthetic_book_with_id(id: u32) -> Book {
    let mut book = synthetic_book();
    book.id = id;
    book
}

pub fn synthetic_book() -> Book {
    let mut book = Book::default();
    book.id = BOOK_ID;
    book.title = BOOK_TITLE.to_string();
    book.author = BOOK_AUTHOR.to_string();
    book.description = BOOK_DESCRIPTION.to_string();
    book.date_published = BOOK_DATE.to_string();
    book.cover_url = COVER_URL.to_string();
    book.cover = Some(cover_bytes());
    book.chapters = vec![story_chapter(), announcement_chapter(), note_chapter()];
    book
}

fn story_chapter() -> Chapter {
    Chapter {
        id: 101,
        date: "2024-01-01".to_string(),
        slug: "chapter-1-the-beginning".to_string(),
        title: CHAPTER_ONE_TITLE.to_string(),
        url: "/fiction/2000000002/chapter/101/chapter-1-the-beginning".to_string(),
        content: Some(format!(
            "<p><span class=\"source-notice\">{NOTICE_TEXT}</span></p>\n\
                 <p><span class=\"copied-only\">{FALSE_NOTICE_TEXT}</span></p>\n\
                 <p class=\"unused-spacer\">\u{00a0}</p>\n\
                 <p style=\"background-color: #f5f5f5;\">The first paragraph has \
                 <a href=\"{PROSE_LINK}\">a prose link</a> that must survive cleanup.</p>\n\
                 <p><img src=\"{STORY_IMAGE_URL}\" alt=\"Story map\" /></p>\n\
                 <p class=\"extra-spacing\">A second paragraph keeps the story readable and \
                 gives the fixture enough prose to score.</p>"
        )),
        ..Chapter::default()
    }
}

fn announcement_chapter() -> Chapter {
    Chapter {
        id: 102,
        date: "2024-01-02".to_string(),
        slug: "announcement-schedule-change".to_string(),
        title: ANNOUNCEMENT_TITLE.to_string(),
        url: "/fiction/2000000002/chapter/102/announcement-schedule-change".to_string(),
        content: Some(
            "<p>The posting schedule is changing. New chapters will arrive every Tuesday.</p>"
                .to_string(),
        ),
        ..Chapter::default()
    }
}

fn note_chapter() -> Chapter {
    Chapter {
        id: 103,
        date: "2024-01-03".to_string(),
        slug: "chapter-2-the-authors-note".to_string(),
        title: CHAPTER_TWO_TITLE.to_string(),
        url: "/fiction/2000000002/chapter/103/chapter-2-the-authors-note".to_string(),
        content: Some(
            "<p>The final scene closes with a quiet moment before the next arc begins.</p>"
                .to_string(),
        ),
        authors_note_end: Some(format!(
            "<p>{NOTE_TEXT} <a href=\"{PROMO_LINK}\">Check out my other story</a>.</p>\n\
             <p><span class=\"source-notice\">{NOTE_NOTICE_TEXT}</span></p>\n\
             <p><img src=\"{PROMO_IMAGE_URL}\" alt=\"Promo cover\" /></p>"
        )),
        ..Chapter::default()
    }
}

fn cover_bytes() -> Vec<u8> {
    encode_image(4, 5, [32, 64, 96], image::ImageOutputFormat::Jpeg(80))
}

fn promo_image_bytes() -> Vec<u8> {
    encode_image(2, 2, [200, 40, 40], image::ImageOutputFormat::Png)
}

fn story_image_png() -> Vec<u8> {
    encode_image(3, 2, [40, 200, 120], image::ImageOutputFormat::Png)
}

fn encode_image(
    width: u32,
    height: u32,
    color: [u8; 3],
    format: image::ImageOutputFormat,
) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(width, height, image::Rgb(color));
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut bytes, format)
        .expect("fixture image should encode");
    bytes.into_inner()
}

/// Seed the inline-image cache so writing the story image never touches the
/// network. The promo image is seeded too, so a regression that downloads it
/// would show up as a written file instead of a silently skipped fetch.
pub fn seed_inline_image_cache(book: &Book) -> eyre::Result<CacheGuard> {
    let cache_dir = Cache::cache_path()?.join(book.id.to_string());
    let _ = std::fs::remove_dir_all(&cache_dir);
    Cache::write_inline_image(book, STORY_IMAGE_FILENAME, &story_image_png())?;
    Cache::write_inline_image(book, PROMO_IMAGE_FILENAME, &promo_image_bytes())?;
    Ok(CacheGuard { cache_dir })
}

/// The bytes seeded for the story image, so tests can compare the archive.
pub fn story_image_bytes() -> Vec<u8> {
    story_image_png()
}

/// Write a source cache (`book.json`) that matches the synthetic fixture.
pub fn write_source_cache(path: &Path, book: &Book) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cached = book.clone();
    cached.cover = None;
    std::fs::write(path, serde_json::to_string_pretty(&cached)?)?;
    Ok(())
}

/// Remove the temporary book cache when it goes out of scope.
pub struct CacheGuard {
    cache_dir: PathBuf,
}

impl Drop for CacheGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cache_dir);
    }
}
