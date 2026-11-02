mod fixture;

use std::collections::HashMap;
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

#[derive(Debug, Default)]
struct XmlNode {
    name: String,
    attributes: HashMap<String, String>,
    text: String,
    children: Vec<XmlNode>,
}

impl XmlNode {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attributes.get(name).map(String::as_str)
    }

    fn all_text(&self) -> String {
        let mut text = self.text.clone();
        for child in &self.children {
            text.push_str(&child.all_text());
        }
        text
    }
}

fn parse_xml(raw: &str) -> eyre::Result<XmlNode> {
    let mut reader = xml::EventReader::new(raw.as_bytes());
    let mut root = XmlNode::default();
    let mut stack: Vec<XmlNode> = Vec::new();

    loop {
        match reader.next()? {
            xml::reader::XmlEvent::StartElement {
                name, attributes, ..
            } => {
                let mut node = XmlNode {
                    name: name.local_name,
                    ..XmlNode::default()
                };
                for attribute in attributes {
                    node.attributes
                        .insert(attribute.name.local_name, attribute.value);
                }
                stack.push(node);
            }
            xml::reader::XmlEvent::EndElement { .. } => {
                let node = stack
                    .pop()
                    .ok_or_else(|| eyre::eyre!("unbalanced closing tag while parsing XML"))?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => root.children.push(node),
                }
            }
            xml::reader::XmlEvent::Characters(text) | xml::reader::XmlEvent::CData(text) => {
                if let Some(current) = stack.last_mut() {
                    current.text.push_str(&text);
                }
            }
            xml::reader::XmlEvent::EndDocument => return Ok(root),
            _ => {}
        }
    }
}

fn find_all<'a>(node: &'a XmlNode, name: &str) -> Vec<&'a XmlNode> {
    let mut matches = Vec::new();
    for child in &node.children {
        if child.name == name {
            matches.push(child);
        }
        matches.extend(find_all(child, name));
    }
    matches
}

fn first<'a>(node: &'a XmlNode, name: &str) -> &'a XmlNode {
    find_all(node, name)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("missing <{name}> element"))
}

fn assert_xml_safe_id(id: &str) {
    let pattern = regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_.-]*$").unwrap();
    assert!(
        pattern.is_match(id),
        "{id:?} is not a valid XML ID (it must start with a letter or underscore)"
    );
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
async fn generated_fixture_epub_has_conformant_package_and_navigation() -> eyre::Result<()> {
    const DESCRIPTION_HTML: &str =
        "<p>A <strong>synthetic</strong> book &amp; fixture for the <em>EPUB</em> writer.</p>";
    const DESCRIPTION_PLAIN: &str = "A synthetic book & fixture for the EPUB writer.";

    let temp = tempfile::tempdir()?;
    let epub_path = temp.path().join("fixture.epub");

    let mut book = fixture::synthetic_book();
    book.title = format!("  {}  ", fixture::BOOK_TITLE);
    book.description = DESCRIPTION_HTML.to_string();

    let _cache_guard = fixture::seed_inline_image_cache(&book)?;
    write_epub(&book, Some(epub_path.to_string_lossy().into_owned())).await?;

    let file = std::fs::File::open(&epub_path)?;
    let mut archive = zip::ZipArchive::new(file)?;

    let opf_raw = archive_text(&mut archive, "OEBPS/content.opf")?;
    assert_well_formed_xml("OEBPS/content.opf", &opf_raw)?;
    let opf = parse_xml(&opf_raw)?;

    let manifest_items = find_all(&opf, "item");

    let cover_item = manifest_items
        .iter()
        .find(|item| item.attr("id") == Some("cover"))
        .expect("cover item is in the manifest");
    assert_eq!(
        cover_item.attr("properties"),
        Some("cover-image"),
        "the cover item carries cover-image semantics"
    );

    let nav_item = manifest_items
        .iter()
        .find(|item| item.attr("properties") == Some("nav"))
        .expect("nav item is in the manifest");
    assert_eq!(nav_item.attr("id"), Some("nav"));
    assert_eq!(nav_item.attr("href"), Some("text/nav.xhtml"));
    assert_eq!(nav_item.attr("media-type"), Some("application/xhtml+xml"));

    let ncx_item = manifest_items
        .iter()
        .find(|item| item.attr("id") == Some("ncx"))
        .expect("NCX item is in the manifest");
    assert_eq!(ncx_item.attr("href"), Some("toc.ncx"));
    assert_eq!(
        ncx_item.attr("media-type"),
        Some("application/x-dtbncx+xml"),
        "the NCX media type is the EPUB 2 fallback type"
    );

    for item in &manifest_items {
        let id = item.attr("id").expect("every manifest item has an id");
        assert_xml_safe_id(id);
    }

    for chapter in &book.chapters {
        let href = format!("text/{}.xhtml", chapter.id);
        let item = manifest_items
            .iter()
            .find(|item| item.attr("href") == Some(href.as_str()))
            .unwrap_or_else(|| panic!("chapter {href} is in the manifest"));
        assert_eq!(item.attr("id"), Some(format!("ch-{}", chapter.id).as_str()));
    }

    let spine = first(&opf, "spine");
    assert_eq!(spine.attr("toc"), Some("ncx"));

    let itemrefs = find_all(spine, "itemref");
    let title_ref = itemrefs
        .iter()
        .find(|itemref| itemref.attr("idref") == Some("title"))
        .expect("the title page is in the spine");
    assert_ne!(title_ref.attr("linear"), Some("no"));

    let nav_ref = itemrefs
        .iter()
        .find(|itemref| itemref.attr("idref") == Some("nav"))
        .expect("the nav document is in the spine");
    assert_eq!(
        nav_ref.attr("linear"),
        Some("no"),
        "the nav document is non-linear"
    );

    for itemref in &itemrefs {
        assert_xml_safe_id(itemref.attr("idref").expect("every itemref has an idref"));
    }

    for chapter in &book.chapters {
        let idref = format!("ch-{}", chapter.id);
        let itemref = itemrefs
            .iter()
            .find(|itemref| itemref.attr("idref") == Some(idref.as_str()))
            .unwrap_or_else(|| panic!("chapter {idref} is in the spine"));
        if chapter.title == fixture::ANNOUNCEMENT_TITLE {
            assert_eq!(
                itemref.attr("linear"),
                Some("no"),
                "the announcement is non-linear"
            );
        } else {
            assert_ne!(
                itemref.attr("linear"),
                Some("no"),
                "story chapters and chapter-titled notices stay linear"
            );
        }
    }

    let ncx_raw = archive_text(&mut archive, "OEBPS/toc.ncx")?;
    assert_well_formed_xml("OEBPS/toc.ncx", &ncx_raw)?;
    let ncx = parse_xml(&ncx_raw)?;

    let nav_point_ids = find_all(&ncx, "navPoint")
        .into_iter()
        .map(|nav_point| nav_point.attr("id").expect("navPoint has an id"))
        .collect::<Vec<_>>();
    assert_eq!(
        nav_point_ids,
        vec!["nav-title", "nav-101", "nav-102", "nav-103"]
    );
    for id in &nav_point_ids {
        assert_xml_safe_id(id);
    }

    let ncx_sources = find_all(&ncx, "content")
        .into_iter()
        .map(|content| content.attr("src").expect("NCX content has a src"))
        .collect::<Vec<_>>();
    assert_eq!(
        ncx_sources,
        vec![
            "text/title.xhtml",
            "text/101.xhtml",
            "text/102.xhtml",
            "text/103.xhtml"
        ],
        "the NCX follows the spine order"
    );

    let nav_raw = archive_text(&mut archive, "OEBPS/text/nav.xhtml")?;
    assert_well_formed_xml("OEBPS/text/nav.xhtml", &nav_raw)?;
    let nav = parse_xml(&nav_raw)?;

    let toc_nav = find_all(&nav, "nav")
        .into_iter()
        .find(|nav| nav.attr("type") == Some("toc"))
        .expect("the nav document has an epub:type=toc nav");
    let links = find_all(toc_nav, "a");
    let hrefs = links
        .iter()
        .map(|link| link.attr("href").expect("toc link has an href"))
        .collect::<Vec<_>>();
    assert_eq!(
        hrefs,
        vec!["title.xhtml", "101.xhtml", "102.xhtml", "103.xhtml"],
        "the nav document follows the spine order"
    );
    let labels = links
        .iter()
        .map(|link| link.all_text().trim().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        vec![
            "Cover",
            fixture::CHAPTER_ONE_TITLE,
            fixture::ANNOUNCEMENT_TITLE,
            fixture::CHAPTER_TWO_TITLE
        ]
    );

    let title = find_all(&opf, "title")
        .into_iter()
        .next()
        .expect("dc:title is in the package metadata");
    assert_eq!(
        title.all_text().trim(),
        fixture::BOOK_TITLE,
        "the package title is trimmed"
    );

    let description = find_all(&opf, "description")
        .into_iter()
        .next()
        .expect("dc:description is in the package metadata");
    assert_eq!(
        description.all_text().trim(),
        DESCRIPTION_PLAIN,
        "the package description is plain text"
    );

    let modified = find_all(&opf, "meta")
        .into_iter()
        .find(|meta| meta.attr("property") == Some("dcterms:modified"))
        .expect("dcterms:modified is in the package metadata");
    let modified = modified.all_text();
    let iso_8601 = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").unwrap();
    assert!(
        iso_8601.is_match(modified.trim()),
        "dcterms:modified {modified:?} is not a UTC ISO 8601 timestamp"
    );

    Ok(())
}
