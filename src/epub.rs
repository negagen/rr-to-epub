use crate::cache::Cache;
use crate::xml_ext::write_elements;
use eyre::bail;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::io::Reader;
use regex::Regex;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;
use uuid::Uuid;
use webp::Decoder;
use xml::writer::XmlEvent;
use xml::EmitterConfig;

const USER_AGENT: &str = "rr-to-epub <https://github.com/isaac-mcfadyen/rr-to-epub>";

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Book {
    pub id: u32,
    pub title: String,
    pub author: String,
    pub description: String,
    pub date_published: String,
    pub cover_url: String,
    pub cover: Option<Vec<u8>>,
    pub chapters: Vec<Chapter>,

    #[serde(skip)]
    client: reqwest::Client,
}
impl Book {
    pub fn id_from_file(path: PathBuf) -> eyre::Result<Option<u32>> {
        // Open the file as a ZIP.
        let mut reader = zip::ZipArchive::new(std::fs::File::open(path)?)?;

        // Open the file at OEBPS/content.opf.
        let mut content_opf = reader.by_name("OEBPS/content.opf")?;

        // Read the file.
        let mut contents = String::new();
        content_opf.read_to_string(&mut contents)?;

        // Parse as XML.
        let mut parsed = xml::EventReader::new(contents.as_bytes());
        loop {
            let Ok(event) = parsed.next() else {
                return Ok(None);
            };
            match event {
                xml::reader::XmlEvent::StartElement { attributes, .. } => {
                    let is_rr_tag = attributes.iter().any(|v| {
                        v.name.local_name == "name" && v.value == "rr-to-epub:royal-road-id"
                    });
                    if is_rr_tag {
                        // Find the content attribute.
                        let Some(content) =
                            attributes.iter().find(|v| v.name.local_name == "content")
                        else {
                            return Ok(None);
                        };

                        let id = content.value.parse::<u32>()?;
                        return Ok(Some(id));
                    }
                }
                xml::reader::XmlEvent::EndDocument => {
                    // End of document without finding the ID.
                    return Ok(None);
                }
                _ => {}
            }
        }
    }
    pub async fn new(id: u32) -> eyre::Result<Self> {
        // Cover in script tag: window.fictionCover = "...";
        let cover_regex = Regex::new(r#"window\.fictionCover = "(.*)";"#).unwrap();
        // Chapters array in script tag: window.chapters = [...];
        let chapters_regex = Regex::new(r#"window\.chapters = (\[.*]);"#).unwrap();
        let client = reqwest::Client::new();

        let request = client
            .get(format!("https://www.royalroad.com/fiction/{}", id))
            .header("User-Agent", USER_AGENT)
            .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Sec-Fetch-Dest", "document")
            .header("Upgrade-Insecure-Requests", "1")
            .send()
            .await?
            .error_for_status()?;
        let response = request.text().await?;

        // Parse book metadata.
        let parsed = Html::parse_document(&response);
        let title_selector = Selector::parse("h1").unwrap();
        let author_selector = Selector::parse("h4 a").unwrap();
        let description_selector = Selector::parse(".description > .hidden-content").unwrap();
        let title = parsed
            .select(&title_selector)
            .next()
            .ok_or(eyre::eyre!("No title found"))?
            .inner_html();
        let author = parsed
            .select(&author_selector)
            .next()
            .ok_or(eyre::eyre!("No author found"))?
            .inner_html();
        let description = parsed
            .select(&description_selector)
            .next()
            .ok_or(eyre::eyre!("No description found"))?
            .inner_html();

        // Parse chapter metadata.
        let cover = cover_regex
            .captures(&response)
            .ok_or(eyre::eyre!("No cover found"))?[1]
            .to_string();
        let chapters = chapters_regex
            .captures(&response)
            .ok_or(eyre::eyre!("No chapters found"))?[1]
            .to_string();
        let chapters: Vec<Chapter> = serde_json::from_str(&chapters)?;

        Ok(Self {
            id,
            cover_url: cover,
            cover: None,
            title,
            author,
            description,
            date_published: chapters.first().unwrap().date.clone(),
            chapters,
            client,
        })
    }
    pub async fn update_cover(&mut self) -> eyre::Result<()> {
        let cover = self
            .client
            .get(&self.cover_url)
            .header("User-Agent", USER_AGENT)
            .send()
            .await?
            .error_for_status()?;
        let bytes = cover.bytes().await?;
        self.cover = Some(bytes.to_vec());
        Ok(())
    }
    pub async fn update_chapter_content(&mut self) -> eyre::Result<()> {
        let num_chapters = self.chapters.len();
        // Select the chapter-inner and chapter-content div.
        let content_selector = Selector::parse(".chapter-inner.chapter-content").unwrap();

        // Select all elements within the content that are not divs with a class (they are the stolen messages).
        let not_stolen_selector = Selector::parse(":scope > :not(div[class])").unwrap();

        // Strange selectors are because RR doesn't have a way to tell if the author's note is
        // at the start or the end in the HTML.
        let authors_note_start_selector = Selector::parse("hr + .portlet > .author-note").unwrap();
        let authors_note_end_selector = Selector::parse("div + .portlet > .author-note").unwrap();

        // Check for existing progress
        let start_index = Cache::read_download_progress(self)?.unwrap_or(0);
        if start_index > 0 {
            tracing::info!(
                "Resuming download from chapter {} of {}",
                start_index + 1,
                num_chapters
            );
        }

        let mut current_index = start_index;
        while current_index < num_chapters {
            let chapter = &mut self.chapters[current_index];
            tracing::info!(
                "Downloading chapter '{}' ({} of {})",
                chapter.title,
                current_index + 1,
                num_chapters
            );
            let url = format!("https://www.royalroad.com{}", chapter.url);
            let request = self
                .client
                .get(url)
                .header("User-Agent", USER_AGENT)
                .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8")
                .header("Accept-Language", "en-US,en;q=0.9")
                .header("Sec-Fetch-Mode", "navigate")
                .header("Sec-Fetch-Site", "none")
                .header("Sec-Fetch-Dest", "document")
                .header("Upgrade-Insecure-Requests", "1")
                .send()
                .await?
                .error_for_status()?;
            let text = request.text().await?;

            let parsed = Html::parse_document(&text);

            // First select content with the content selector.
            let content = parsed
                .select(&content_selector)
                .next()
                .ok_or(eyre::eyre!("No content found"))?;

            // Then pull out non-stolen items.
            let content = content
                .select(&not_stolen_selector)
                .map(|v| v.html())
                .collect::<Vec<_>>()
                .join("\n");
            chapter.content = Some(content);

            // Parse starting AN.
            if let Some(authors_note) = parsed.select(&authors_note_start_selector).next() {
                let authors_note = authors_note.inner_html();
                if !authors_note.is_empty() {
                    chapter.authors_note_start = Some(authors_note);
                }
            }
            // Parse ending AN.
            if let Some(authors_note) = parsed.select(&authors_note_end_selector).next() {
                let authors_note = authors_note.inner_html();
                if !authors_note.is_empty() {
                    chapter.authors_note_end = Some(authors_note);
                }
            }

            // Save progress every 100 chapters
            if (current_index + 1) % 100 == 0 {
                tracing::info!(
                    "Saving checkpoint at chapter {} of {}",
                    current_index + 1,
                    num_chapters
                );
                Cache::save_download_progress(self, current_index + 1)?;
                Cache::write_book(self)?;
            }

            // Sleep for 0.5 seconds to avoid rate limiting.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;

            current_index += 1;
        }

        // Clear the progress file when done
        let cache_dir = Cache::cache_path()?.join(self.id.to_string());
        let progress_file = cache_dir.join("download_progress.json");
        if progress_file.exists() {
            std::fs::remove_file(progress_file)?;
        }

        Ok(())
    }
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
pub struct Chapter {
    pub id: u32,
    pub date: String,
    pub slug: String,
    pub title: String,
    pub url: String,
    pub content: Option<String>,

    pub authors_note_start: Option<String>,
    pub authors_note_end: Option<String>,
}

/// A document in reading order, used to keep the nav document and the NCX in
/// sync with the package spine.
struct SpineDocument {
    manifest_id: String,
    ncx_id: String,
    title: String,
    href: String,
    nav_href: String,
    linear: bool,
}

fn spine_documents(book: &Book) -> Vec<SpineDocument> {
    let mut documents = Vec::with_capacity(book.chapters.len() + 2);
    documents.push(SpineDocument {
        manifest_id: "title".to_string(),
        ncx_id: "nav-title".to_string(),
        title: "Cover".to_string(),
        href: "text/title.xhtml".to_string(),
        nav_href: "title.xhtml".to_string(),
        linear: true,
    });
    documents.push(SpineDocument {
        manifest_id: "notice".to_string(),
        ncx_id: "nav-notice".to_string(),
        title: "Notice".to_string(),
        href: "text/notice.xhtml".to_string(),
        nav_href: "notice.xhtml".to_string(),
        linear: true,
    });

    for chapter in &book.chapters {
        documents.push(SpineDocument {
            manifest_id: format!("ch-{}", chapter.id),
            ncx_id: format!("nav-{}", chapter.id),
            title: chapter.title.trim().to_string(),
            href: format!("text/{}.xhtml", chapter.id),
            nav_href: format!("{}.xhtml", chapter.id),
            linear: !is_non_story_document(&chapter.title),
        });
    }

    documents
}

/// Match the QA facts heuristic: announcement, update, schedule, and notice
/// documents are non-linear unless the title also names a chapter.
fn is_non_story_document(title: &str) -> bool {
    let title = title.to_lowercase();
    if title.contains("chapter") {
        return false;
    }
    ["announcement", "update", "schedule", "notice"]
        .iter()
        .any(|keyword| title.contains(keyword))
}

fn plain_text(html: &str) -> String {
    let fragment = Html::parse_fragment(html);
    let text = fragment.root_element().text().collect::<Vec<_>>().join(" ");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn utc_modified() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let days = (seconds / 86_400) as i64;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert days since the Unix epoch to a proleptic Gregorian date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;

    (if month <= 2 { year + 1 } else { year }, month, day)
}

pub const STRUCTURAL_CLASSES: [&str; 4] = [
    "chapter-title",
    "chapter-content",
    "authors-note-start",
    "authors-note-end",
];

const NOTICE_KEYWORDS: [&str; 15] = [
    "author",
    "chapter",
    "source",
    "copy",
    "copied",
    "consent",
    "permission",
    "royalroad",
    "royal road",
    "support",
    "official",
    "publish",
    "reproduce",
    "readers",
    "work",
];

/// Chapter content cleaned once and reused for both XHTML output and image discovery.
struct CleanedChapter {
    content: Option<String>,
    authors_note_start: Option<String>,
    authors_note_end: Option<String>,
}

/// A cleaned chapter waiting for the package to be written.
struct PreparedChapter<'a> {
    chapter: &'a Chapter,
    cleaned: CleanedChapter,
}

/// An inline image that is both referenced by a chapter and written to the EPUB.
struct ImageResource {
    filename: String,
    manifest_id: String,
    media_type: &'static str,
    bytes: Vec<u8>,
}

impl ImageResource {
    fn href(&self) -> String {
        format!("images/{}", self.filename)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FragmentKind {
    ChapterContent,
    AuthorsNote,
}

fn clean_chapter(chapter: &Chapter) -> eyre::Result<CleanedChapter> {
    Ok(CleanedChapter {
        content: chapter
            .content
            .as_deref()
            .map(|content| clean_fragment(content, FragmentKind::ChapterContent))
            .transpose()?,
        authors_note_start: chapter
            .authors_note_start
            .as_deref()
            .map(|note| clean_fragment(note, FragmentKind::AuthorsNote))
            .transpose()?,
        authors_note_end: chapter
            .authors_note_end
            .as_deref()
            .map(|note| clean_fragment(note, FragmentKind::AuthorsNote))
            .transpose()?,
    })
}

fn clean_fragment(body: &str, kind: FragmentKind) -> eyre::Result<String> {
    let mut document = Html::parse_fragment(body);

    match kind {
        FragmentKind::ChapterContent => remove_source_notices(&mut document),
        FragmentKind::AuthorsNote => remove_author_note_promos(&mut document),
    }
    remove_empty_paragraphs(&mut document);
    clean_attributes(&mut document);

    Ok(serialize_fragment(&document))
}

fn keyword_present(text: &str, keyword: &str) -> bool {
    let mut start = 0;
    while let Some(offset) = text[start..].find(keyword) {
        let position = start + offset;
        let before = text[..position].chars().next_back();
        let after = text[position + keyword.len()..].chars().next();
        let is_boundary = |character: Option<char>| {
            character.is_none_or(|character| !character.is_alphanumeric())
        };
        if is_boundary(before) && is_boundary(after) {
            return true;
        }
        start = position + keyword.len();
    }
    false
}

fn is_source_notice(text: &str) -> bool {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let length = normalized.chars().count();
    if !(80..=260).contains(&length) {
        return false;
    }

    let lowercase = normalized.to_lowercase();
    let matches = NOTICE_KEYWORDS
        .iter()
        .filter(|keyword| keyword_present(&lowercase, keyword))
        .count();
    matches >= 2
}

fn remove_source_notices(document: &mut Html) {
    let ids: Vec<_> = document
        .tree
        .nodes()
        .filter_map(|node| {
            let element = scraper::ElementRef::wrap(node)?;
            if element.value().name() == "span" {
                let text = element.text().collect::<String>();
                is_source_notice(&text).then(|| node.id())
            } else {
                None
            }
        })
        .collect();

    for id in ids {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
}

fn remove_author_note_promos(document: &mut Html) {
    let image_ids: Vec<_> = document
        .tree
        .nodes()
        .filter_map(|node| {
            let element = scraper::ElementRef::wrap(node)?;
            (element.value().name() == "img").then(|| node.id())
        })
        .collect();
    for id in image_ids {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }

    let anchor_ids: Vec<_> = document
        .tree
        .nodes()
        .filter_map(|node| {
            let element = scraper::ElementRef::wrap(node)?;
            (element.value().name() == "a").then(|| node.id())
        })
        .collect();
    for id in anchor_ids {
        if let Some(mut node) = document.tree.get_mut(id) {
            if let scraper::Node::Element(element) = node.value() {
                element
                    .attrs
                    .retain(|name, _| name.local.as_ref() != "href");
            }
        }
    }
}

fn remove_empty_paragraphs(document: &mut Html) {
    let image_selector = Selector::parse("img").unwrap();
    let ids: Vec<_> = document
        .tree
        .nodes()
        .filter_map(|node| {
            let element = scraper::ElementRef::wrap(node)?;
            if element.value().name() != "p" {
                return None;
            }

            let text = element.text().collect::<String>().replace('\u{00a0}', " ");
            let has_image = element.select(&image_selector).next().is_some();
            (text.trim().is_empty() && !has_image).then(|| node.id())
        })
        .collect();

    for id in ids {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
}

fn clean_attributes(document: &mut Html) {
    let ids: Vec<_> = document
        .tree
        .nodes()
        .filter(|node| node.value().is_element())
        .map(|node| node.id())
        .collect();

    for id in ids {
        let Some(mut node) = document.tree.get_mut(id) else {
            continue;
        };
        let scraper::Node::Element(element) = node.value() else {
            continue;
        };

        element
            .attrs
            .retain(|name, _| name.local.as_ref() != "style");

        let class_value = element
            .attrs
            .iter()
            .find(|(name, _)| name.local.as_ref() == "class")
            .map(|(_, value)| value.to_string());
        let Some(class_value) = class_value else {
            continue;
        };

        let retained = class_value
            .split_whitespace()
            .filter(|class| STRUCTURAL_CLASSES.contains(class))
            .collect::<Vec<_>>()
            .join(" ");
        if retained.is_empty() {
            element
                .attrs
                .retain(|name, _| name.local.as_ref() != "class");
        } else if let Some((_, value)) = element
            .attrs
            .iter_mut()
            .find(|(name, _)| name.local.as_ref() == "class")
        {
            *value = retained.as_str().into();
        }
    }
}

fn is_void_element(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn escape_xml(output: &mut String, value: &str, attribute: bool) {
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' if attribute => output.push_str("&quot;"),
            _ => output.push(character),
        }
    }
}

/// Serialize a parsed fragment as XML-safe XHTML, self-closing void elements.
fn serialize_fragment(document: &Html) -> String {
    let root = document.tree.root();
    let mut output = String::new();
    let mut stack: Vec<_> = root.children().map(|node| (node, false)).collect();
    stack.reverse();

    while let Some((node, closing)) = stack.pop() {
        if closing {
            if let Some(element) = node.value().as_element() {
                if !is_void_element(element.name()) {
                    output.push_str("</");
                    output.push_str(element.name());
                    output.push('>');
                }
            }
            continue;
        }

        match node.value() {
            // The fragment parser wraps body content in html/body elements;
            // those wrappers are not part of the chapter fragment.
            scraper::Node::Element(element) if matches!(element.name(), "html" | "body") => {
                let children: Vec<_> = node.children().collect();
                for child in children.into_iter().rev() {
                    stack.push((child, false));
                }
            }
            scraper::Node::Element(element) => {
                output.push('<');
                output.push_str(element.name());
                for (name, value) in element.attrs.iter() {
                    output.push(' ');
                    output.push_str(name.local.as_ref());
                    output.push_str("=\"");
                    escape_xml(&mut output, value, true);
                    output.push('"');
                }

                if is_void_element(element.name()) {
                    output.push_str(" />");
                } else {
                    output.push('>');
                    stack.push((node, true));
                    let children: Vec<_> = node.children().collect();
                    for child in children.into_iter().rev() {
                        stack.push((child, false));
                    }
                }
            }
            scraper::Node::Text(text) => escape_xml(&mut output, text, false),
            _ => {}
        }
    }

    output
}

pub async fn write_epub(
    book: &Book,
    outfile: Option<String>,
    authorized_copy: bool,
) -> eyre::Result<()> {
    // Create a temp dir.
    let temp_folder = tempfile::tempdir()?;

    // Choose the filename.
    let outfile = match outfile {
        Some(outfile) => outfile,
        None => format!(
            "{}.epub",
            book.title
                .replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "-")
        ),
    };

    // Clean every chapter before writing the package. Image discovery uses the
    // cleaned fragments, so removed promo images are never downloaded.
    let prepared = book
        .chapters
        .iter()
        .map(|chapter| {
            Ok(PreparedChapter {
                chapter,
                cleaned: clean_chapter(chapter)?,
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;

    let mut image_urls = Vec::new();
    let mut seen_urls = HashSet::new();
    for prepared_chapter in &prepared {
        for url in cleaned_image_urls(&prepared_chapter.cleaned)? {
            if seen_urls.insert(url.clone()) {
                image_urls.push(url);
            }
        }
    }

    let mut images = Vec::new();
    let mut image_names = HashMap::new();
    let mut used_filenames = HashSet::new();
    let mut used_ids = HashSet::new();
    for url in image_urls {
        let Some(bytes) = load_image(book, &url).await? else {
            tracing::warn!("Skipping inline image that could not be loaded: {}", url);
            continue;
        };
        let Some((media_type, extension)) = image_type(&bytes) else {
            tracing::warn!("Skipping inline image with an unknown media type: {}", url);
            continue;
        };
        let filename = unique_image_filename(&url, extension, &mut used_filenames)?;
        let manifest_id = unique_image_manifest_id(&filename, &mut used_ids);
        image_names.insert(url, filename.clone());
        images.push(ImageResource {
            filename,
            manifest_id,
            media_type,
            bytes,
        });
    }

    let (cover_filename, cover_media_type) = cover_resource(book);

    // Open the file.
    let epub_path = temp_folder
        .path()
        .join(Uuid::new_v4().to_string())
        .with_extension("epub");
    tracing::debug!("Writing epub to {:?}", epub_path);
    let file = std::fs::File::create(&epub_path)?;
    let mut epub_file = zip::ZipWriter::new(file);

    // Write the mimetype first and uncompressed, as OCF readers require.
    epub_file.start_file(
        "mimetype",
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored),
    )?;
    epub_file.write_all(b"application/epub+zip")?;

    // Write the META-INF folder.
    epub_file.add_directory("META-INF", zip::write::FileOptions::default())?;

    // Write the container.xml file.
    epub_file.start_file("META-INF/container.xml", zip::write::FileOptions::default())?;
    container_xml(book, &mut epub_file)?;

    // Write the content.opf file, including every retained image.
    epub_file.start_file("OEBPS/content.opf", zip::write::FileOptions::default())?;
    content_opf(
        book,
        &images,
        &cover_filename,
        cover_media_type,
        &mut epub_file,
    )?;

    // Write the table of contents (toc.ncx) file.
    epub_file.start_file("OEBPS/toc.ncx", zip::write::FileOptions::default())?;
    toc_ncx(book, &mut epub_file)?;

    // Write each chapter with its ImageResource references rewritten to the
    // retained image files.
    for prepared_chapter in &prepared {
        epub_file.start_file(
            format!("OEBPS/text/{}.xhtml", prepared_chapter.chapter.id),
            zip::write::FileOptions::default(),
        )?;
        chapter_html(
            prepared_chapter.chapter,
            &prepared_chapter.cleaned,
            &image_names,
            &mut epub_file,
        )?;
    }

    // Write only the retained images.
    for image in &images {
        epub_file.start_file(
            format!("OEBPS/images/{}", image.filename),
            zip::write::FileOptions::default(),
        )?;
        epub_file.write_all(&image.bytes)?;
    }

    // Write the cover, preferring bytes that were already fetched.
    epub_file.start_file(
        format!("OEBPS/images/{cover_filename}"),
        zip::write::FileOptions::default(),
    )?;
    match &book.cover {
        Some(cover) => epub_file.write_all(cover)?,
        None => download_cover(book, &mut epub_file).await?,
    }

    // Write the title page.
    epub_file.start_file("OEBPS/text/title.xhtml", zip::write::FileOptions::default())?;
    title_html(book, &mut epub_file)?;

    // Write the one-page attribution notice.
    epub_file.start_file(
        "OEBPS/text/notice.xhtml",
        zip::write::FileOptions::default(),
    )?;
    notice_html(book, authorized_copy, &mut epub_file)?;

    // Write the EPUB 3 navigation document.
    epub_file.start_file("OEBPS/text/nav.xhtml", zip::write::FileOptions::default())?;
    nav_html(book, &mut epub_file)?;

    // Write the stylesheet.
    epub_file.start_file(
        "OEBPS/styles/stylesheet.css",
        zip::write::FileOptions::default(),
    )?;
    stylesheet(&mut epub_file)?;

    // Finish and copy to user destination.
    epub_file.finish()?;
    tracing::debug!("Copying epub from {:?} to {:?}", epub_path, outfile);
    std::fs::copy(epub_path, &outfile)?;

    tracing::info!("Wrote EPUB to {:?}", outfile);
    Ok(())
}

fn cleaned_image_urls(cleaned: &CleanedChapter) -> eyre::Result<Vec<String>> {
    let mut images = parse_images(cleaned.content.as_deref().unwrap_or(""))?;
    images.extend(parse_images(
        cleaned.authors_note_start.as_deref().unwrap_or(""),
    )?);
    images.extend(parse_images(
        cleaned.authors_note_end.as_deref().unwrap_or(""),
    )?);
    Ok(images)
}

fn stylesheet(file: &mut impl Write) -> eyre::Result<()> {
    file.write_all(include_bytes!("./assets/styles.css"))?;
    Ok(())
}

fn title_html(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);

    // Write the body
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("html")
                .ns("", "http://www.w3.org/1999/xhtml")
                .into(),
            // Write the head.
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("title").into(),
            XmlEvent::characters(&book.title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("link")
                .attr("rel", "stylesheet")
                .attr("type", "text/css")
                .attr("href", "../styles/stylesheet.css")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("body").into(),
            XmlEvent::start_element("h1").into(),
            XmlEvent::characters(&book.title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("h2").into(),
            XmlEvent::characters(&book.author),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn notice_html(book: &Book, authorized_copy: bool, file: &mut impl Write) -> eyre::Result<()> {
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);
    let fiction_url = format!("https://www.royalroad.com/fiction/{}", book.id);
    let status = if authorized_copy {
        "The author has authorized EPUB copies."
    } else {
        "This copy is unauthorized."
    };

    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("html")
                .ns("", "http://www.w3.org/1999/xhtml")
                .attr("xml:lang", "en")
                .into(),
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("title").into(),
            XmlEvent::characters("Notice"),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("link")
                .attr("rel", "stylesheet")
                .attr("type", "text/css")
                .attr("href", "../styles/stylesheet.css")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("body").into(),
            XmlEvent::start_element("h1").into(),
            XmlEvent::characters("Notice"),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("p").into(),
            XmlEvent::characters(&format!(
                "This is a downloaded copy of the Royal Road publication \"{}\" by {}.",
                book.title.trim(),
                book.author
            )),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("p").into(),
            XmlEvent::characters("Please support the author on Royal Road: "),
            XmlEvent::start_element("a")
                .attr("href", &fiction_url)
                .into(),
            XmlEvent::characters(&fiction_url),
            XmlEvent::end_element().into(),
            XmlEvent::characters("."),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("p").into(),
            XmlEvent::characters(status),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn chapter_html(
    chapter: &Chapter,
    cleaned: &CleanedChapter,
    image_names: &HashMap<String, String>,
    file: &mut impl Write,
) -> eyre::Result<()> {
    let mut xml = EmitterConfig::new().perform_indent(true);
    xml.perform_escaping = false;
    let mut xml = xml.create_writer(file);

    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("html")
                .ns("", "http://www.w3.org/1999/xhtml")
                .attr("xml:lang", "en")
                .into(),
            // Write the head.
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("title").into(),
            XmlEvent::characters(&chapter.title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "generator")
                .attr("content", "text/html; charset=UTF-8")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("link")
                .attr("href", "../styles/stylesheet.css")
                .attr("rel", "stylesheet")
                .attr("type", "text/css")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            // Write the body.
            XmlEvent::start_element("body").into(),
            XmlEvent::start_element("h1")
                .attr("class", "chapter-title")
                .into(),
            XmlEvent::characters(&chapter.title),
            XmlEvent::end_element().into(),
        ],
    )?;

    // Write the starting author's note, if any.
    if let Some(authors_note_start) = cleaned.authors_note_start.as_deref() {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("div")
                    .attr("class", "authors-note-start")
                    .into(),
                XmlEvent::characters(&rewrite_images(
                    authors_note_start.to_string(),
                    image_names,
                )?),
                XmlEvent::end_element().into(),
            ],
        )?;
    }
    // Write the content.
    if let Some(content) = cleaned.content.as_deref() {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("div")
                    .attr("class", "chapter-content")
                    .into(),
                // Rewrite the images to be pointing to our downloaded ones.
                XmlEvent::characters(&rewrite_images(content.to_string(), image_names)?),
                XmlEvent::end_element().into(),
            ],
        )?;
    }
    // Write the ending author's note, if any.
    if let Some(authors_note_end) = cleaned.authors_note_end.as_deref() {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("div")
                    .attr("class", "authors-note-end")
                    .into(),
                XmlEvent::characters(&rewrite_images(authors_note_end.to_string(), image_names)?),
                XmlEvent::end_element().into(),
            ],
        )?;
    }

    // Close elements.
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn container_xml(_: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);

    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("container")
                .attr("version", "1.0")
                .ns("", "urn:oasis:names:tc:opendocument:xmlns:container")
                .into(),
            XmlEvent::start_element("rootfiles").into(),
            XmlEvent::start_element("rootfile")
                .attr("full-path", "OEBPS/content.opf")
                .attr("media-type", "application/oebps-package+xml")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;
    Ok(())
}

fn content_opf(
    book: &Book,
    images: &[ImageResource],
    cover_filename: &str,
    cover_media_type: &str,
    file: &mut impl Write,
) -> eyre::Result<()> {
    let documents = spine_documents(book);
    let title = book.title.trim();
    let description = plain_text(&book.description);
    let modified = utc_modified();

    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("package")
                .ns("", "http://www.idpf.org/2007/opf")
                .attr("version", "3.0")
                .attr("unique-identifier", "bookid")
                .into(),
            XmlEvent::start_element("metadata")
                .ns("dc", "http://purl.org/dc/elements/1.1/")
                .ns("dcterms", "http://purl.org/dc/terms/")
                .into(),
            XmlEvent::start_element("dc:title").into(),
            XmlEvent::characters(title),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:creator").into(),
            XmlEvent::characters(&book.author),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:description").into(),
            XmlEvent::characters(&description),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:date").into(),
            XmlEvent::characters(&book.date_published),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:identifier")
                .attr("id", "bookid")
                .into(),
            XmlEvent::characters(&book.id.to_string()),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("dc:language").into(),
            XmlEvent::characters("en"),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("property", "dcterms:modified")
                .into(),
            XmlEvent::characters(&modified),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "cover")
                .attr("content", "cover")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "primary-writing-mode")
                .attr("content", "horizontal-lr")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "rr-to-epub:royal-road-id")
                .attr("content", &book.id.to_string())
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            // Write the manifest.
            XmlEvent::start_element("manifest").into(),
            // Write the cover.
            XmlEvent::start_element("item")
                .attr("id", "cover")
                .attr("href", &format!("images/{cover_filename}"))
                .attr("media-type", cover_media_type)
                .attr("properties", "cover-image")
                .into(),
            XmlEvent::end_element().into(),
            // Write the stylesheet.
            XmlEvent::start_element("item")
                .attr("id", "stylesheet")
                .attr("href", "styles/stylesheet.css")
                .attr("media-type", "text/css")
                .into(),
            XmlEvent::end_element().into(),
            // Write the table of contents.
            XmlEvent::start_element("item")
                .attr("id", "ncx")
                .attr("href", "toc.ncx")
                .attr("media-type", "application/x-dtbncx+xml")
                .into(),
            XmlEvent::end_element().into(),
            // Write the EPUB 3 navigation document.
            XmlEvent::start_element("item")
                .attr("id", "nav")
                .attr("href", "text/nav.xhtml")
                .attr("media-type", "application/xhtml+xml")
                .attr("properties", "nav")
                .into(),
            XmlEvent::end_element().into(),
        ],
    )?;

    // Declare every retained inline image with a stable ID and real media type.
    for image in images {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("item")
                    .attr("id", &image.manifest_id)
                    .attr("href", &image.href())
                    .attr("media-type", image.media_type)
                    .into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }

    // Write the title page and each chapter in reading order.
    for document in &documents {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("item")
                    .attr("id", &document.manifest_id)
                    .attr("href", &document.href)
                    .attr("media-type", "application/xhtml+xml")
                    .into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }

    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            // Start the spine.
            XmlEvent::start_element("spine").attr("toc", "ncx").into(),
        ],
    )?;

    for document in &documents {
        let itemref = XmlEvent::start_element("itemref").attr("idref", &document.manifest_id);
        let itemref = if document.linear {
            itemref
        } else {
            itemref.attr("linear", "no")
        };
        write_elements(
            &mut xml,
            vec![itemref.into(), XmlEvent::end_element().into()],
        )?;
    }

    // The navigation document itself must not take part in the linear reading
    // order.
    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("itemref")
                .attr("idref", "nav")
                .attr("linear", "no")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;

    Ok(())
}

fn toc_ncx(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let documents = spine_documents(book);
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);

    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("ncx")
                .ns("", "http://www.daisy.org/z3986/2005/ncx/")
                .attr("version", "2005-1")
                .into(),
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:uid")
                .attr("content", &format!("{}", book.id))
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:depth")
                .attr("content", "2")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:totalPageCount")
                .attr("content", "0")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("meta")
                .attr("name", "dtb:maxPageNumber")
                .attr("content", "0")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("docTitle").into(),
            XmlEvent::start_element("text").into(),
            XmlEvent::characters(book.title.trim()),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("navMap").into(),
        ],
    )?;

    for (index, document) in documents.iter().enumerate() {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("navPoint")
                    .attr("id", &document.ncx_id)
                    .attr("playOrder", &format!("{}", index + 1))
                    .into(),
                XmlEvent::start_element("navLabel").into(),
                XmlEvent::start_element("text").into(),
                XmlEvent::characters(&document.title),
                XmlEvent::end_element().into(),
                XmlEvent::end_element().into(),
                XmlEvent::start_element("content")
                    .attr("src", &document.href)
                    .into(),
                XmlEvent::end_element().into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }

    // Write the end of the document.
    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;

    Ok(())
}

fn nav_html(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let documents = spine_documents(book);
    let mut xml = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(file);

    write_elements(
        &mut xml,
        vec![
            XmlEvent::start_element("html")
                .ns("", "http://www.w3.org/1999/xhtml")
                .ns("epub", "http://www.idpf.org/2007/ops")
                .attr("xml:lang", "en")
                .into(),
            XmlEvent::start_element("head").into(),
            XmlEvent::start_element("title").into(),
            XmlEvent::characters("Contents"),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("link")
                .attr("rel", "stylesheet")
                .attr("type", "text/css")
                .attr("href", "../styles/stylesheet.css")
                .into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("body").into(),
            XmlEvent::start_element("nav")
                .attr("epub:type", "toc")
                .attr("id", "toc")
                .into(),
            XmlEvent::start_element("h1").into(),
            XmlEvent::characters("Contents"),
            XmlEvent::end_element().into(),
            XmlEvent::start_element("ol").into(),
        ],
    )?;

    for document in &documents {
        write_elements(
            &mut xml,
            vec![
                XmlEvent::start_element("li").into(),
                XmlEvent::start_element("a")
                    .attr("href", &document.nav_href)
                    .into(),
                XmlEvent::characters(&document.title),
                XmlEvent::end_element().into(),
                XmlEvent::end_element().into(),
            ],
        )?;
    }

    write_elements(
        &mut xml,
        vec![
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
            XmlEvent::end_element().into(),
        ],
    )?;

    Ok(())
}

fn extract_image_name(url: &str) -> eyre::Result<String> {
    let mut url = Url::parse(url)?;
    url.set_query(None);
    url.set_fragment(None);

    Ok(url
        .path_segments()
        .ok_or(eyre::eyre!("Invalid image URL"))?
        .next_back()
        .ok_or(eyre::eyre!("Invalid image URL"))?
        .to_string())
}
fn parse_images(body: &str) -> eyre::Result<Vec<String>> {
    let parsed = Html::parse_fragment(body);
    let selector = Selector::parse("img").unwrap();

    let mut sources = Vec::new();
    for element in parsed.select(&selector) {
        if let Some(src) = element.value().attr("src") {
            sources.push(src.to_string());
        }
    }
    Ok(sources)
}
fn rewrite_images(body: String, image_names: &HashMap<String, String>) -> eyre::Result<String> {
    let mut document = Html::parse_fragment(&body);
    let ids: Vec<_> = document
        .tree
        .nodes()
        .filter_map(|node| {
            let element = scraper::ElementRef::wrap(node)?;
            (element.value().name() == "img").then(|| node.id())
        })
        .collect();

    for id in ids {
        let Some(mut node) = document.tree.get_mut(id) else {
            continue;
        };
        let src = node
            .value()
            .as_element()
            .and_then(|element| element.attr("src"))
            .map(str::to_string);
        let Some(src) = src else {
            node.detach();
            continue;
        };
        match image_names.get(&src) {
            Some(filename) => {
                let new_src = format!("../images/{filename}");
                if let scraper::Node::Element(element) = node.value() {
                    if let Some((_, value)) = element
                        .attrs
                        .iter_mut()
                        .find(|(name, _)| name.local.as_ref() == "src")
                    {
                        *value = new_src.as_str().into();
                    }
                }
            }
            None => node.detach(),
        }
    }

    Ok(serialize_fragment(&document))
}

fn is_png(bytes: &[u8]) -> bool {
    bytes.len() > 8
        && bytes[0] == 0x89
        && bytes[1] == 0x50
        && bytes[2] == 0x4E
        && bytes[3] == 0x47
        && bytes[4] == 0x0D
        && bytes[5] == 0x0A
        && bytes[6] == 0x1A
        && bytes[7] == 0x0A
}
fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF
}
fn is_webp(bytes: &[u8]) -> bool {
    bytes.len() > 11
        && bytes[0] == 0x52
        && bytes[1] == 0x49
        && bytes[2] == 0x46
        && bytes[3] == 0x46
        && bytes[8] == 0x57
        && bytes[9] == 0x45
        && bytes[10] == 0x42
        && bytes[11] == 0x50
}
fn is_gif(bytes: &[u8]) -> bool {
    bytes.len() > 3 && bytes[0] == 0x47 && bytes[1] == 0x49 && bytes[2] == 0x46 && bytes[3] == 0x38
}
fn is_svg(bytes: &[u8]) -> bool {
    if let Ok(text) = std::str::from_utf8(bytes) {
        text.to_lowercase().trim().starts_with("<?xml")
            || text.to_lowercase().trim().starts_with("<svg")
    } else {
        false
    }
}
fn is_html(bytes: &[u8]) -> bool {
    if let Ok(text) = std::str::from_utf8(bytes) {
        text.to_lowercase().trim().starts_with("<!doctype html>")
            || text.to_lowercase().trim().starts_with("<html")
    } else {
        false
    }
}

fn image_type(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if is_png(bytes) {
        Some(("image/png", "png"))
    } else if is_jpeg(bytes) {
        Some(("image/jpeg", "jpg"))
    } else if is_gif(bytes) {
        Some(("image/gif", "gif"))
    } else if is_svg(bytes) {
        Some(("image/svg+xml", "svg"))
    } else {
        None
    }
}

fn cover_resource(book: &Book) -> (String, &'static str) {
    if let Some((media_type, extension)) = book.cover.as_deref().and_then(image_type) {
        let extension = if extension == "jpg" {
            "jpeg"
        } else {
            extension
        };
        (format!("cover.{extension}"), media_type)
    } else {
        ("cover.jpeg".to_string(), "image/jpeg")
    }
}

fn unique_image_filename(
    url: &str,
    extension: &str,
    used: &mut HashSet<String>,
) -> eyre::Result<String> {
    let original = extract_image_name(url)?;
    let stem = Path::new(&original)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .unwrap_or("image");

    let mut candidate = format!("{stem}.{extension}");
    let mut suffix = 2;
    while !used.insert(candidate.clone()) {
        candidate = format!("{stem}-{suffix}.{extension}");
        suffix += 1;
    }
    Ok(candidate)
}

fn unique_image_manifest_id(filename: &str, used: &mut HashSet<String>) -> String {
    let mut base = String::from("img-");
    for character in filename.chars() {
        if character.is_ascii_alphanumeric() {
            base.push(character.to_ascii_lowercase());
        } else if !base.ends_with('-') {
            base.push('-');
        }
    }
    while base.ends_with('-') {
        base.pop();
    }
    if base == "img" {
        base.push_str("-image");
    }

    let mut candidate = base.clone();
    let mut suffix = 2;
    while !used.insert(candidate.clone()) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// Load one inline image, preferring the cache so tests and repeat runs never
/// touch the network.
async fn load_image(book: &Book, url: &str) -> eyre::Result<Option<Vec<u8>>> {
    let filename = extract_image_name(url)?;
    if let Some(image) = Cache::read_inline_image(book, &filename)? {
        return Ok(Some(image.to_vec()));
    }

    let client = reqwest::Client::new();
    let response = match client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            // Handle DNS and connection errors gracefully
            tracing::warn!(
                "Failed to download image from URL ({}): {}. This is likely NOT a bug with rr-to-epub.",
                url,
                error
            );
            return Ok(None);
        }
    };

    if !response.status().is_success() {
        tracing::warn!(
            "Failed to download image from URL (HTTP {}): {}. This is likely NOT a bug with rr-to-epub.",
            response.status(),
            url
        );
        return Ok(None);
    }
    let bytes = response.bytes().await?;

    if is_html(&bytes) {
        // Skip HTML pages.
        return Ok(None);
    }
    if is_gif(&bytes) || is_svg(&bytes) {
        // Directly pass through the image without changing the cache policy.
        return Ok(Some(bytes.to_vec()));
    }

    let image = if is_webp(&bytes) {
        // Decode the image.
        let Some(webp) = Decoder::new(&bytes).decode() else {
            tracing::warn!(
                "Failed to decode webp image. Please report this as a bug and include the following URL: {}",
                url
            );
            return Ok(None); // Skip the image.
        };
        webp.to_image()
    } else if is_jpeg(&bytes) || is_png(&bytes) {
        // Decode the image.
        Reader::new(Cursor::new(&bytes))
            .with_guessed_format()?
            .decode()?
    } else {
        bail!("Unsupported inline image format. Please report this as a bug and include the following URL: {}", url);
    };

    // Resize to max width of 600px.
    let width = image.width();
    let height = image.height();
    let image = image.resize(
        600,
        600 * height / width,
        image::imageops::FilterType::Lanczos3,
    );

    // Encode the image.
    let mut buffer = Vec::new();
    if is_png(&bytes) || is_webp(&bytes) {
        // We write both PNG and WebP as PNG because WebP is not supported by some e-readers.
        image.write_with_encoder(PngEncoder::new_with_quality(
            Cursor::new(&mut buffer),
            CompressionType::Fast,
            FilterType::Adaptive,
        ))?;
    } else if is_jpeg(&bytes) {
        image.write_with_encoder(JpegEncoder::new_with_quality(Cursor::new(&mut buffer), 80))?;
    } else {
        bail!("Unsupported inline image format. Please report this as a bug and include the following URL: {}", url);
    };

    // Save the image in the cache.
    Cache::write_inline_image(book, &filename, &buffer)?;

    Ok(Some(buffer))
}

async fn download_cover(book: &Book, file: &mut impl Write) -> eyre::Result<()> {
    let client = reqwest::Client::new();
    let cover = client
        .get(&book.cover_url)
        .header("User-Agent", USER_AGENT)
        .send()
        .await?
        .error_for_status()?;
    let bytes = cover.bytes().await?;
    file.write_all(&bytes)?;
    Ok(())
}
