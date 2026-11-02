mod fixture;

use std::io::Read;

use rr_to_epub::epub::write_epub;

fn archive_text(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> eyre::Result<String> {
    let mut text = String::new();
    archive.by_name(name)?.read_to_string(&mut text)?;
    Ok(text)
}

fn assert_well_formed_xml(name: &str, raw: &str) -> eyre::Result<()> {
    let mut reader = xml::EventReader::new(raw.as_bytes());
    loop {
        match reader.next() {
            Ok(xml::reader::XmlEvent::EndDocument) => return Ok(()),
            Ok(_) => {}
            Err(error) => return Err(eyre::eyre!("{name} is not well-formed XML: {error}")),
        }
    }
}

#[test]
fn fixture_book_carries_the_raw_cases_later_tickets_clean() {
    let book = fixture::synthetic_book();

    let story = book.chapters[0]
        .content
        .as_deref()
        .expect("story chapter has content");
    assert!(story.contains("class=\"source-notice\""));
    assert!(story.contains(fixture::NOTICE_TEXT));
    assert!(story.contains('\u{00a0}'));
    assert!(story.contains("class=\"unused-spacer\""));
    assert!(story.contains("style=\"background-color"));
    assert!(story.contains(fixture::PROSE_LINK));

    assert_eq!(book.chapters[1].title, fixture::ANNOUNCEMENT_TITLE);

    let note = book.chapters[2]
        .authors_note_end
        .as_deref()
        .expect("author's note chapter has an ending note");
    assert!(note.contains(fixture::PROMO_IMAGE_URL));
    assert!(note.contains(fixture::PROMO_LINK));
    assert!(note.contains(fixture::NOTE_TEXT));

    assert!(book.cover.is_some(), "cover bytes are supplied directly");
}

#[tokio::test]
async fn generated_fixture_epub_opens_with_expected_chapters_and_titles() -> eyre::Result<()> {
    let temp = tempfile::tempdir()?;
    let epub_path = temp.path().join("fixture.epub");
    let source_cache_path = temp.path().join("book.json");

    let book = fixture::synthetic_book();
    let _cache_guard = fixture::seed_inline_image_cache(&book)?;
    fixture::write_source_cache(&source_cache_path, &book)?;

    write_epub(&book, Some(epub_path.to_string_lossy().into_owned())).await?;
    assert!(epub_path.exists(), "writer produced an EPUB file");

    let file = std::fs::File::open(&epub_path)?;
    let mut archive = zip::ZipArchive::new(file)?;
    assert!(archive.by_name("mimetype").is_ok());
    assert!(archive.by_name("META-INF/container.xml").is_ok());

    let opf = archive_text(&mut archive, "OEBPS/content.opf")?;
    assert_well_formed_xml("OEBPS/content.opf", &opf)?;

    let mut cover = Vec::new();
    archive
        .by_name("OEBPS/images/cover.jpeg")?
        .read_to_end(&mut cover)?;
    assert_eq!(
        cover,
        book.cover.as_deref().expect("fixture cover bytes"),
        "the writer used the supplied cover bytes"
    );

    let mut titles = Vec::new();
    for chapter in &book.chapters {
        let name = format!("OEBPS/text/{}.xhtml", chapter.id);
        let raw = archive_text(&mut archive, &name)?;
        assert_well_formed_xml(&name, &raw)?;

        let document = scraper::Html::parse_document(&raw);
        let selector = scraper::Selector::parse("h1").expect("valid h1 selector");
        let title = document
            .select(&selector)
            .next()
            .map(|heading| heading.text().collect::<String>())
            .unwrap_or_default();
        titles.push(title.trim().to_string());
    }

    assert_eq!(titles, fixture::expected_chapter_titles());
    assert_eq!(titles.len(), 3);

    let cached: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&source_cache_path)?)?;
    assert_eq!(cached["id"], fixture::BOOK_ID);
    assert_eq!(
        cached["chapters"]
            .as_array()
            .expect("source cache chapters")
            .len(),
        fixture::expected_chapter_titles().len()
    );

    Ok(())
}

#[tokio::test]
async fn generated_fixture_epub_cleans_chapter_content_and_notes() -> eyre::Result<()> {
    const STRUCTURAL_CLASSES: [&str; 4] = [
        "chapter-title",
        "chapter-content",
        "authors-note-start",
        "authors-note-end",
    ];

    let temp = tempfile::tempdir()?;
    let epub_path = temp.path().join("fixture.epub");

    let book = fixture::synthetic_book();
    let _cache_guard = fixture::seed_inline_image_cache(&book)?;
    write_epub(&book, Some(epub_path.to_string_lossy().into_owned())).await?;

    let file = std::fs::File::open(&epub_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    let story_raw = archive_text(&mut archive, "OEBPS/text/101.xhtml")?;
    assert_well_formed_xml("OEBPS/text/101.xhtml", &story_raw)?;
    assert!(
        !story_raw.contains(fixture::NOTICE_TEXT),
        "the injected source notice is removed from chapter content"
    );
    assert!(
        !story_raw.contains("source-notice"),
        "the source-notice class is removed"
    );
    assert!(
        story_raw.contains(fixture::PROSE_LINK),
        "prose links survive cleanup"
    );
    assert!(
        story_raw.contains("a prose link"),
        "prose link text survives cleanup"
    );
    assert!(
        !story_raw.contains("unused-spacer") && !story_raw.contains("extra-spacing"),
        "non-structural classes are removed"
    );
    assert!(
        !story_raw.contains("style=") && !story_raw.contains("background-color"),
        "inline styles are removed"
    );
    assert_eq!(
        story_raw.matches("<html").count(),
        1,
        "cleaned content is not wrapped in a nested html element"
    );

    let story = scraper::Html::parse_document(&story_raw);
    let paragraph_selector = scraper::Selector::parse("p").expect("valid p selector");
    let image_selector = scraper::Selector::parse("img").expect("valid img selector");
    for paragraph in story.select(&paragraph_selector) {
        let text = paragraph
            .text()
            .collect::<String>()
            .replace('\u{00a0}', " ");
        assert!(
            !text.trim().is_empty() || paragraph.select(&image_selector).next().is_some(),
            "no empty paragraph remains in chapter content"
        );
    }

    let class_selector = scraper::Selector::parse("[class]").expect("valid class selector");
    for element in story.select(&class_selector) {
        let class = element
            .value()
            .attr("class")
            .expect("class selector matched an element with class");
        for token in class.split_whitespace() {
            assert!(
                STRUCTURAL_CLASSES.contains(&token),
                "unexpected class {token:?} remains in chapter content"
            );
        }
    }
    let style_selector = scraper::Selector::parse("[style]").expect("valid style selector");
    assert_eq!(
        story.select(&style_selector).count(),
        0,
        "no inline style attribute remains in chapter content"
    );

    let note_raw = archive_text(&mut archive, "OEBPS/text/103.xhtml")?;
    assert_well_formed_xml("OEBPS/text/103.xhtml", &note_raw)?;
    assert!(
        note_raw.contains(fixture::NOTE_TEXT),
        "author-note text survives cleanup"
    );
    assert!(
        note_raw.contains(fixture::NOTE_NOTICE_TEXT),
        "the source-notice rule never runs inside an author-note container"
    );
    assert!(
        note_raw.contains("Check out my other story"),
        "author-note link text survives cleanup"
    );
    assert!(
        !note_raw.contains(fixture::PROMO_LINK),
        "author-note link targets are removed"
    );
    assert!(
        !note_raw.contains(fixture::PROMO_IMAGE_URL) && !note_raw.contains("promo.png"),
        "author-note promo images are removed"
    );

    let note = scraper::Html::parse_document(&note_raw);
    assert_eq!(
        note.select(&image_selector).count(),
        0,
        "author notes contain no images after cleanup"
    );
    let anchor_selector = scraper::Selector::parse("a").expect("valid a selector");
    for anchor in note.select(&anchor_selector) {
        assert!(
            anchor.value().attr("href").is_none(),
            "author-note link targets are removed"
        );
    }
    assert!(
        archive.by_name("OEBPS/images/promo.png").is_err(),
        "promo images are never written to the EPUB"
    );

    let css = archive_text(&mut archive, "OEBPS/styles/stylesheet.css")?;
    assert!(
        css.contains("margin: 0 0 0.55em"),
        "paragraphs use the compact bottom margin"
    );
    assert!(
        css.contains("line-height: 1.45"),
        "paragraphs use the compact line height"
    );
    assert!(
        css.contains("p:empty") && css.contains("display: none"),
        "empty paragraphs are hidden"
    );
    assert!(
        css.contains("max-width: 100%"),
        "images are capped at the page width"
    );

    Ok(())
}
