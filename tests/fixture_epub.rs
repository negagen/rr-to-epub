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
