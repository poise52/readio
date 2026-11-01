use crate::{Book, Chapter, Error, Result};
use base64::{Engine, engine::general_purpose::STANDARD};
use percent_encoding::percent_decode_str;
use roxmltree::{Document, Node};
use scraper::{Html, Selector};
use std::{
    collections::HashMap,
    fs::File,
    io::{Cursor, Read},
    path::Path,
};
use uuid::Uuid;

const LIMIT: u64 = 80 * 1024 * 1024;
const COVER_LIMIT: usize = 2 * 1024 * 1024;

/// Imports a supported book from disk.
///
/// # Errors
/// Returns an error for unreadable, oversized, unsupported, or malformed files.
pub fn import_book(path: &Path) -> Result<Book> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() > LIMIT {
        return Err(Error::Format("Книга больше 80 МБ".into()));
    }
    let mut bytes = Vec::new();
    file.by_ref().take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(Error::Format("Книга больше 80 МБ".into()));
    }
    let title = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Книга");
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut book = match extension.as_str() {
        "txt" => Book {
            title: title.into(),
            author: "Автор не указан".into(),
            chapters: vec![Chapter {
                title: "Текст".into(),
                text: clean(&decode(&bytes)),
            }],
            ..Book::default()
        },
        "fb2" => fb2(title, &decode(&bytes))?,
        "epub" => epub(title, &bytes)?,
        _ => return Err(Error::Format("Поддерживаются EPUB, FB2 и TXT".into())),
    };
    if book.chapters.iter().all(|c| c.text.is_empty()) {
        return Err(Error::Format("В книге не найден текст".into()));
    }
    book.id = Uuid::new_v4().to_string();
    book.refresh();
    Ok(book)
}
fn decode(bytes: &[u8]) -> String {
    std::str::from_utf8(bytes).map_or_else(
        |_| encoding_rs::WINDOWS_1251.decode(bytes).0.into_owned(),
        |s| s.trim_start_matches('\u{feff}').to_owned(),
    )
}
fn clean(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    let mut newlines = 0;
    for c in text.replace("\r\n", "\n").chars() {
        match c {
            ' ' | '\t' => {
                if !space {
                    out.push(' ');
                }
                space = true;
                newlines = 0;
            }
            '\n' | '\r' => {
                if newlines < 2 {
                    out.push('\n');
                }
                newlines += 1;
                space = false;
            }
            _ => {
                out.push(c);
                space = false;
                newlines = 0;
            }
        }
    }
    out.trim().to_owned()
}
fn text(node: Node<'_, '_>) -> String {
    node.descendants()
        .filter_map(|n| n.text().filter(|_| n.is_text()))
        .collect::<String>()
        .trim()
        .to_owned()
}
fn first(node: Node<'_, '_>, name: &str) -> Option<String> {
    node.descendants()
        .find(|n| n.has_tag_name(name))
        .map(text)
        .filter(|s| !s.is_empty())
}
fn paragraphs(node: Node<'_, '_>, skip_heading: Option<roxmltree::NodeId>) -> String {
    clean(
        &node
            .descendants()
            .filter(|n| {
                n.is_element()
                    && !n
                        .ancestors()
                        .any(|ancestor| Some(ancestor.id()) == skip_heading)
                    && matches!(n.tag_name().name(), "p" | "v" | "subtitle")
                    && !n
                        .descendants()
                        .skip(1)
                        .any(|c| c.is_element() && matches!(c.tag_name().name(), "p" | "v"))
            })
            .map(text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
    )
}
fn fb2(fallback: &str, source: &str) -> Result<Book> {
    let doc = Document::parse(source).map_err(|e| Error::Format(e.to_string()))?;
    let root = doc.root_element();
    let mut book = Book {
        title: first(root, "book-title").unwrap_or_else(|| fallback.into()),
        author: "Автор не указан".into(),
        ..Book::default()
    };
    if let Some(author) = root.descendants().find(|n| n.has_tag_name("author")) {
        let name = ["first-name", "middle-name", "last-name"]
            .iter()
            .filter_map(|n| first(author, n))
            .collect::<Vec<_>>()
            .join(" ");
        if !name.is_empty() {
            book.author = name;
        }
    }
    for body in root
        .descendants()
        .filter(|n| n.has_tag_name("body") && n.attribute("name").is_none())
    {
        let sections: Vec<_> = body
            .children()
            .filter(|n| n.has_tag_name("section"))
            .collect();
        if sections.is_empty() {
            let content = paragraphs(body, None);
            if !content.is_empty() {
                book.chapters.push(Chapter {
                    title: "Текст".into(),
                    text: content,
                });
            }
        }
        for section in sections {
            let heading = section.children().find(|n| n.has_tag_name("title"));
            let content = paragraphs(section, heading.map(|n| n.id()));
            if !content.is_empty() {
                let title = heading
                    .map(text)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("Глава {}", book.chapters.len() + 1));
                book.chapters.push(Chapter {
                    title,
                    text: content,
                });
            }
        }
    }
    let cover_id = root
        .descendants()
        .find(|n| n.has_tag_name("coverpage"))
        .and_then(|n| n.descendants().find(|n| n.has_tag_name("image")))
        .and_then(|n| n.attributes().find(|a| a.name() == "href"))
        .and_then(|a| a.value().strip_prefix('#'));
    if let Some(image) = root.descendants().find(|n| {
        n.has_tag_name("binary")
            && cover_id.is_some_and(|id| n.attribute("id") == Some(id))
            && n.attribute("content-type")
                .is_some_and(|s| s.starts_with("image/"))
    }) {
        let encoded: String = text(image).chars().filter(|c| !c.is_whitespace()).collect();
        if let Ok(bytes) = STANDARD.decode(encoded)
            && bytes.len() < COVER_LIMIT
        {
            book.cover = Some(STANDARD.encode(bytes));
        }
    }
    Ok(book)
}
fn zip_read(
    zip: &mut zip::ZipArchive<Cursor<&[u8]>>,
    name: &str,
    remaining: &mut u64,
) -> Result<Vec<u8>> {
    let mut file = zip
        .by_name(name)
        .map_err(|e| Error::Format(format!("EPUB {name}: {e}")))?;
    if file.size() > *remaining {
        return Err(Error::Format("Распакованный EPUB больше 80 МБ".into()));
    }
    let mut bytes = Vec::new();
    file.by_ref().take(*remaining + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > *remaining {
        return Err(Error::Format("Распакованный EPUB больше 80 МБ".into()));
    }
    *remaining -= bytes.len() as u64;
    Ok(bytes)
}
fn resolve(base: &str, href: &str) -> Result<String> {
    let decoded = percent_decode_str(href.split('#').next().unwrap_or(""))
        .decode_utf8()
        .map_err(|e| Error::Format(e.to_string()))?;
    let mut parts = Vec::new();
    for part in base.split('/').chain(decoded.split('/')) {
        match part {
            "" | "." => (),
            ".." => {
                if parts.pop().is_none() {
                    return Err(Error::Format("Недопустимый путь EPUB".into()));
                }
            }
            _ => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}
#[allow(clippy::too_many_lines)] // Keep package and spine processing in one bounded archive pass.
fn epub(fallback: &str, bytes: &[u8]) -> Result<Book> {
    let mut zip =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| Error::Format(e.to_string()))?;
    let mut remaining = LIMIT;
    let container = decode(&zip_read(
        &mut zip,
        "META-INF/container.xml",
        &mut remaining,
    )?);
    let doc = Document::parse(&container).map_err(|e| Error::Format(e.to_string()))?;
    let opf_path = doc
        .descendants()
        .find(|n| n.has_tag_name("rootfile"))
        .and_then(|n| n.attribute("full-path"))
        .ok_or_else(|| Error::Format("Не найден EPUB package".into()))?;
    let opf = decode(&zip_read(&mut zip, opf_path, &mut remaining)?);
    let doc = Document::parse(&opf).map_err(|e| Error::Format(e.to_string()))?;
    let root = doc.root_element();
    let base = opf_path.rsplit_once('/').map_or("", |(p, _)| p);
    let mut book = Book {
        title: first(root, "title").unwrap_or_else(|| fallback.into()),
        author: first(root, "creator").unwrap_or_else(|| "Автор не указан".into()),
        ..Book::default()
    };
    let manifest: HashMap<_, _> = doc
        .descendants()
        .filter(|n| n.has_tag_name("item"))
        .filter_map(|n| Some((n.attribute("id")?, n)))
        .collect();
    let blocks =
        Selector::parse("p,h1,h2,h3,blockquote,li").map_err(|e| Error::Format(e.to_string()))?;
    let heading = Selector::parse("h1,h2,h3").map_err(|e| Error::Format(e.to_string()))?;
    let body_selector = Selector::parse("body").map_err(|e| Error::Format(e.to_string()))?;
    for itemref in doc.descendants().filter(|n| n.has_tag_name("itemref")) {
        let Some(item) = itemref.attribute("idref").and_then(|id| manifest.get(id)) else {
            continue;
        };
        let Some(href) = item.attribute("href") else {
            continue;
        };
        let data = zip_read(&mut zip, &resolve(base, href)?, &mut remaining)?;
        let html = Html::parse_document(&decode(&data));
        let Some(body) = html.select(&body_selector).next() else {
            continue;
        };
        let chapter_heading = body.select(&heading).find(|element| {
            !element.text().collect::<String>().trim().is_empty()
                && !element.ancestors().any(|ancestor| {
                    ancestor
                        .value()
                        .as_element()
                        .is_some_and(|node| matches!(node.name(), "nav" | "script" | "style"))
                })
        });
        let heading_id = chapter_heading.map(|element| element.id());
        let title = chapter_heading.map_or_else(
            || format!("Глава {}", book.chapters.len() + 1),
            |element| element.text().collect::<String>().trim().to_owned(),
        );
        let parts: Vec<_> = body
            .select(&blocks)
            .filter(|element| Some(element.id()) != heading_id)
            .filter(|e| {
                !e.ancestors().any(|a| {
                    a.value()
                        .as_element()
                        .is_some_and(|e| matches!(e.name(), "nav" | "script" | "style"))
                })
            })
            .filter(|e| {
                !e.descendants().skip(1).any(|n| {
                    n.value().as_element().is_some_and(|n| {
                        matches!(n.name(), "p" | "h1" | "h2" | "h3" | "blockquote" | "li")
                    })
                })
            })
            .map(|e| e.text().collect::<String>())
            .collect();
        let content = clean(&if parts.is_empty() {
            body.descendants()
                .filter(|n| {
                    n.value().as_text().is_some()
                        && !n
                            .ancestors()
                            .any(|ancestor| Some(ancestor.id()) == heading_id)
                        && !n.ancestors().any(|a| {
                            a.value()
                                .as_element()
                                .is_some_and(|a| matches!(a.name(), "nav" | "script" | "style"))
                        })
                })
                .filter_map(|n| n.value().as_text().map(|t| t.to_string()))
                .collect()
        } else {
            parts.join("\n\n")
        });
        if !content.is_empty() {
            book.chapters.push(Chapter {
                title,
                text: content,
            });
        }
    }
    for item in manifest.values() {
        if (item
            .attribute("properties")
            .is_some_and(|p| p.split_whitespace().any(|v| v == "cover-image"))
            || item
                .attribute("id")
                .is_some_and(|s| s.to_ascii_lowercase().contains("cover")))
            && item
                .attribute("media-type")
                .is_some_and(|s| s.starts_with("image/"))
        {
            if let Some(href) = item.attribute("href") {
                let data = zip_read(&mut zip, &resolve(base, href)?, &mut remaining)?;
                if data.len() < COVER_LIMIT {
                    book.cover = Some(STANDARD.encode(data));
                }
            }
            break;
        }
    }
    Ok(book)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fb2_extracts_chapters() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let b = fb2(
            "x",
            "<FictionBook><description><title-info><book-title>Тест</book-title></title-info></description><body><section><title><p>Глава</p></title><p>Привет</p><section><title><p>Подглава</p></title><subtitle>Подзаголовок</subtitle><p>Продолжение</p></section></section></body></FictionBook>",
        )?;
        assert_eq!(b.title, "Тест");
        assert_eq!(b.chapters[0].title, "Глава");
        assert_eq!(
            b.chapters[0].text,
            "Привет\n\nПодглава\n\nПодзаголовок\n\nПродолжение"
        );
        Ok(())
    }
    #[test]
    fn resolves_paths_without_escape() -> std::result::Result<(), Box<dyn std::error::Error>> {
        assert_eq!(
            resolve("OEBPS/Text", "../cover%20page.xhtml")?,
            "OEBPS/cover page.xhtml"
        );
        assert!(resolve("", "../secret").is_err());
        Ok(())
    }
    #[test]
    fn epub_follows_spine_and_deduplicates_blocks()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        let cursor = Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default();
        for (name, content) in [
            (
                "META-INF/container.xml",
                r#"<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container"><rootfiles><rootfile full-path="OEBPS/book.opf"/></rootfiles></container>"#,
            ),
            (
                "OEBPS/book.opf",
                r#"<package xmlns="http://www.idpf.org/2007/opf"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>Test</dc:title></metadata><manifest><item id="a" href="a.xhtml"/><item id="b" href="b.xhtml"/></manifest><spine><itemref idref="b"/><itemref idref="a"/></spine></package>"#,
            ),
            ("OEBPS/a.xhtml", "<html><body><p>Second</p></body></html>"),
            (
                "OEBPS/b.xhtml",
                "<html><body><nav><h1>Skip</h1></nav><h1>Chapter</h1><blockquote><p>First</p></blockquote><h2>Subheading</h2><p>More</p></body></html>",
            ),
        ] {
            zip.start_file(name, options)?;
            zip.write_all(content.as_bytes())?;
        }
        let bytes = zip.finish()?.into_inner();
        let book = epub("fallback", &bytes)?;
        assert_eq!(book.title, "Test");
        assert_eq!(book.chapters[0].title, "Chapter");
        assert_eq!(
            book.chapters
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>(),
            ["First\n\nSubheading\n\nMore", "Second"]
        );
        assert!(epub("x", b"not a zip").is_err());
        Ok(())
    }
    #[test]
    fn archive_budget_rejects_oversized_entry()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("chapter", zip::write::SimpleFileOptions::default())?;
        zip.write_all(b"12345")?;
        let bytes = zip.finish()?.into_inner();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.as_slice()))?;
        assert!(zip_read(&mut archive, "chapter", &mut 4).is_err());
        Ok(())
    }
    #[test]
    fn decodes_cp1251() {
        assert_eq!(decode(&[0xcf, 0xf0, 0xe8, 0xe2, 0xe5, 0xf2]), "Привет");
    }
}
