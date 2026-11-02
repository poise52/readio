use crate::{Book, BookSummary, Chapter, Error, Mark, Preferences, Result, import_book};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Debug)]
pub struct Store {
    conn: Connection,
    root: PathBuf,
}
impl Store {
    /// # Errors
    /// Returns an error if the directory or database cannot be opened.
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)?;
        let conn = Connection::open(root.join("readio.sqlite3"))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
          CREATE TABLE IF NOT EXISTS books (id TEXT PRIMARY KEY,title TEXT NOT NULL,author TEXT NOT NULL,cover TEXT,chapter INTEGER NOT NULL,offset INTEGER NOT NULL,speed REAL NOT NULL,total INTEGER NOT NULL,done INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS chapters (book_id TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,idx INTEGER NOT NULL,title TEXT NOT NULL,text TEXT NOT NULL,length INTEGER NOT NULL,PRIMARY KEY(book_id,idx));
          CREATE TABLE IF NOT EXISTS marks (id TEXT PRIMARY KEY,book_id TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,chapter INTEGER NOT NULL,offset INTEGER NOT NULL,excerpt TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY,data TEXT NOT NULL);")?;
        Ok(Self {
            conn,
            root: root.to_owned(),
        })
    }
    fn insert(tx: &Transaction<'_>, book: &Book) -> Result<bool> {
        let total: usize = book
            .chapters
            .iter()
            .map(|c| c.text.encode_utf16().count())
            .sum();
        let done: usize = book
            .chapters
            .iter()
            .take(book.chapter)
            .map(|c| c.text.encode_utf16().count())
            .sum::<usize>()
            + book.offset;
        if tx.execute(
            "INSERT OR IGNORE INTO books VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                book.id,
                book.title,
                book.author,
                book.cover,
                book.chapter,
                book.offset,
                book.speed,
                total,
                done
            ],
        )? == 0
        {
            return Ok(false);
        }
        for (idx, chapter) in book.chapters.iter().enumerate() {
            tx.execute(
                "INSERT INTO chapters VALUES (?1,?2,?3,?4,?5)",
                params![
                    book.id,
                    idx,
                    chapter.title,
                    chapter.text,
                    chapter.text.encode_utf16().count()
                ],
            )?;
        }
        for mark in &book.marks {
            tx.execute(
                "INSERT INTO marks VALUES (?1,?2,?3,?4,?5)",
                params![mark.id, book.id, mark.chapter, mark.offset, mark.excerpt],
            )?;
        }
        Ok(true)
    }
    /// # Errors
    /// Returns an error if the library metadata cannot be read.
    pub fn books(&self) -> Result<Vec<BookSummary>> {
        let mut stmt = self.conn.prepare("SELECT id,title,author,cover,chapter,offset,speed,CASE WHEN total=0 THEN 0.0 ELSE 1.0*done/total END FROM books ORDER BY rowid DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(BookSummary {
                id: r.get(0)?,
                title: r.get(1)?,
                author: r.get(2)?,
                cover: r.get(3)?,
                chapter: r.get(4)?,
                offset: r.get(5)?,
                speed: r.get(6)?,
                progress: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
    /// # Errors
    /// Returns an error if the bookmarks cannot be read.
    pub fn bookmarks(&self) -> Result<Vec<crate::BookmarkEntry>> {
        let mut stmt = self.conn.prepare("SELECT b.id,b.title,b.author,b.cover,b.chapter,b.offset,b.speed,CASE WHEN b.total=0 THEN 0.0 ELSE 1.0*b.done/b.total END,m.id,m.chapter,m.offset,m.excerpt,c.title FROM marks m JOIN books b ON b.id=m.book_id JOIN chapters c ON c.book_id=m.book_id AND c.idx=m.chapter ORDER BY m.rowid DESC")?;
        let rows = stmt.query_map([], |r| {
            Ok(crate::BookmarkEntry {
                book: BookSummary {
                    id: r.get(0)?,
                    title: r.get(1)?,
                    author: r.get(2)?,
                    cover: r.get(3)?,
                    chapter: r.get(4)?,
                    offset: r.get(5)?,
                    speed: r.get(6)?,
                    progress: r.get(7)?,
                },
                mark: Mark {
                    id: r.get(8)?,
                    chapter: r.get(9)?,
                    offset: r.get(10)?,
                    excerpt: r.get(11)?,
                },
                chapter_title: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
    /// # Errors
    /// Returns an error if the book is absent or its data cannot be read.
    pub fn book(&self, id: &str) -> Result<Book> {
        let mut book = self
            .conn
            .query_row(
                "SELECT id,title,author,cover,chapter,offset,speed FROM books WHERE id=?1",
                [id],
                |r| {
                    Ok(Book {
                        id: r.get(0)?,
                        title: r.get(1)?,
                        author: r.get(2)?,
                        cover: r.get(3)?,
                        chapter: r.get(4)?,
                        offset: r.get(5)?,
                        speed: r.get(6)?,
                        ..Book::default()
                    })
                },
            )
            .optional()?
            .ok_or(Error::NotFound)?;
        let mut stmt = self
            .conn
            .prepare("SELECT title,text FROM chapters WHERE book_id=?1 ORDER BY idx")?;
        book.chapters = stmt
            .query_map([id], |r| {
                Ok(Chapter {
                    title: r.get(0)?,
                    text: r.get(1)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        let mut stmt = self.conn.prepare(
            "SELECT id,chapter,offset,excerpt FROM marks WHERE book_id=?1 ORDER BY rowid",
        )?;
        book.marks = stmt
            .query_map([id], |r| {
                Ok(Mark {
                    id: r.get(0)?,
                    chapter: r.get(1)?,
                    offset: r.get(2)?,
                    excerpt: r.get(3)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        book.refresh();
        Ok(book)
    }
    /// # Errors
    /// Returns an error if a book cannot be parsed or stored.
    pub fn import(&self, paths: &[String]) -> Result<Vec<BookSummary>> {
        let books: Vec<_> = paths
            .iter()
            .map(|p| import_book(Path::new(p)))
            .collect::<Result<_>>()?;
        self.import_parsed(&books)
    }
    /// # Errors
    /// Returns an error if the database transaction fails.
    pub fn import_parsed(&self, books: &[Book]) -> Result<Vec<BookSummary>> {
        let tx = self.conn.unchecked_transaction()?;
        for book in books {
            Self::insert(&tx, book)?;
        }
        tx.commit()?;
        Ok(books.iter().map(Book::summary).collect())
    }
    /// # Errors
    /// Returns an error if the book is absent or cannot be deleted.
    pub fn delete(&self, id: &str) -> Result<()> {
        if self.conn.execute("DELETE FROM books WHERE id=?1", [id])? == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }
    /// # Errors
    /// Returns an error if the position is invalid or cannot be stored.
    pub fn position(
        &self,
        id: &str,
        chapter: usize,
        offset: usize,
        speed: f32,
    ) -> Result<BookSummary> {
        if !speed.is_finite() || !(0.25..=4.0).contains(&speed) {
            return Err(Error::Format("Недопустимая скорость".into()));
        }
        let length: usize = self
            .conn
            .query_row(
                "SELECT length FROM chapters WHERE book_id=?1 AND idx=?2",
                params![id, chapter],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::Format("Глава не найдена".into()))?;
        let done: usize = self.conn.query_row(
            "SELECT COALESCE(SUM(length),0) FROM chapters WHERE book_id=?1 AND idx<?2",
            params![id, chapter],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "UPDATE books SET chapter=?2,offset=?3,speed=?4,done=?5 WHERE id=?1",
            params![
                id,
                chapter,
                offset.min(length),
                speed,
                done + offset.min(length)
            ],
        )?;
        self.conn.query_row("SELECT id,title,author,cover,chapter,offset,speed,CASE WHEN total=0 THEN 0.0 ELSE 1.0*done/total END FROM books WHERE id=?1", [id], |r| Ok(BookSummary { id:r.get(0)?,title:r.get(1)?,author:r.get(2)?,cover:r.get(3)?,chapter:r.get(4)?,offset:r.get(5)?,speed:r.get(6)?,progress:r.get(7)? })).optional()?.ok_or(Error::NotFound)
    }
    /// # Errors
    /// Returns an error if the chapter is absent or the bookmark cannot be stored.
    pub fn add_mark(&self, id: &str, chapter: usize, offset: usize) -> Result<Mark> {
        let text: String = self
            .conn
            .query_row(
                "SELECT text FROM chapters WHERE book_id=?1 AND idx=?2",
                params![id, chapter],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::Format("Глава не найдена".into()))?;
        let offset = offset.min(text.encode_utf16().count());
        let excerpt = crate::split_passages(&text)
            .into_iter()
            .find(|p| p.end > offset)
            .map_or_else(String::new, |p| p.text.chars().take(150).collect());
        let mark = Mark {
            id: Uuid::new_v4().to_string(),
            chapter,
            offset,
            excerpt,
        };
        self.conn.execute(
            "INSERT INTO marks VALUES (?1,?2,?3,?4,?5)",
            params![mark.id, id, chapter, offset, mark.excerpt],
        )?;
        Ok(mark)
    }
    /// # Errors
    /// Returns an error if the bookmark is absent or cannot be deleted.
    pub fn delete_mark(&self, id: &str, mark_id: &str) -> Result<()> {
        if self.conn.execute(
            "DELETE FROM marks WHERE book_id=?1 AND id=?2",
            params![id, mark_id],
        )? == 0
        {
            return Err(Error::NotFound);
        }
        Ok(())
    }
    /// # Errors
    /// Returns an error if the stored settings cannot be read.
    pub fn preferences(&self) -> Result<Preferences> {
        let data: Option<String> = self
            .conn
            .query_row(
                "SELECT data FROM settings WHERE key='preferences'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        data.map_or_else(
            || Ok(Preferences::default()),
            |s| {
                let mut prefs: Preferences = serde_json::from_str(&s)?;
                prefs.volume = prefs.volume.clamp(0.0, 1.0);
                Ok(prefs)
            },
        )
    }
    /// # Errors
    /// Returns an error if the settings are invalid or cannot be stored.
    pub fn save_preferences(&self, prefs: &Preferences) -> Result<Preferences> {
        if !prefs.volume.is_finite() || !(0.0..=1.0).contains(&prefs.volume) {
            return Err(Error::Format("Громкость должна быть от 0 до 1".into()));
        }
        if ![
            (prefs.font_size, 10.0, 60.0),
            (prefs.line_height, 0.8, 3.0),
            (prefs.background_opacity, 0.0, 1.0),
            (prefs.background_dim, 0.0, 0.8),
            (prefs.background_blur, 0.0, 1.0),
            (prefs.surface_transparency, 0.0, 0.75),
        ]
        .iter()
        .all(|(v, min, max)| v.is_finite() && (*min..=*max).contains(v))
        {
            return Err(Error::Format("Недопустимые настройки оформления".into()));
        }
        self.conn.execute("INSERT INTO settings (key,data) VALUES ('preferences',?1) ON CONFLICT(key) DO UPDATE SET data=excluded.data", [serde_json::to_string(prefs)?])?;
        Ok(prefs.clone())
    }
    /// # Errors
    /// Returns an error if the image cannot be copied or the settings cannot be stored.
    pub fn background(&self, path: Option<&str>) -> Result<Preferences> {
        let mut prefs = self.preferences()?;
        let old = prefs.background_image_path.clone();
        let target = if let Some(path) = path {
            let source = Path::new(path);
            let ext = source
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("jpg")
                .to_ascii_lowercase();
            if !["png", "jpg", "jpeg", "webp", "gif", "bmp"].contains(&ext.as_str()) {
                return Err(Error::Format("Неподдерживаемый формат изображения".into()));
            }
            let target = self
                .root
                .join(format!("background-{}.{}", Uuid::new_v4(), ext));
            fs::copy(source, &target)?;
            Some(target)
        } else {
            None
        };
        prefs.background_image_path = target.as_ref().map(|p| p.to_string_lossy().into_owned());
        if let Err(e) = self.save_preferences(&prefs) {
            if let Some(target) = target {
                let _ = fs::remove_file(target);
            }
            return Err(e);
        }
        if let Some(old) = old {
            let old = Path::new(&old);
            if old.parent() == Some(self.root.as_path())
                && old
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("background-"))
            {
                let _ = fs::remove_file(old);
            }
        }
        Ok(prefs)
    }
    /// # Errors
    /// Returns an error if the legacy data is malformed, unreadable, or cannot be stored.
    #[allow(clippy::too_many_lines)] // Validate all legacy data before the atomic database transaction.
    pub fn import_legacy(&self, root: &Path) -> Result<usize> {
        fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
            use std::io::Read;
            let file = match fs::File::open(path) {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    fs::File::open(path.with_extension("json.bak"))?
                }
                Err(e) => return Err(e.into()),
            };
            let mut bytes = Vec::new();
            file.take(limit + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > limit {
                return Err(Error::Format(
                    "Файл старой библиотеки слишком большой".into(),
                ));
            }
            Ok(bytes)
        }
        let index: serde_json::Value =
            serde_json::from_slice(&read(&root.join("index.json"), 8 * 1024 * 1024)?)?;
        let mut prefs: Preferences = serde_json::from_value(
            index
                .get("prefs")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        )?;
        prefs.font_size = prefs.font_size.clamp(10.0, 60.0);
        prefs.line_height = prefs.line_height.clamp(0.8, 3.0);
        prefs.background_opacity = prefs.background_opacity.clamp(0.0, 1.0);
        prefs.background_dim = prefs.background_dim.clamp(0.0, 0.8);
        prefs.background_blur = prefs.background_blur.clamp(0.0, 1.0);
        prefs.surface_transparency = prefs.surface_transparency.clamp(0.0, 0.75);
        prefs.volume = prefs.volume.clamp(0.0, 1.0);
        prefs.voice = prefs.voice.as_deref().and_then(|v| {
            if v.starts_with("Edge Дмитрий") {
                Some("ru-RU-DmitryNeural".to_owned())
            } else if v.starts_with("Edge Светлана") {
                Some("ru-RU-SvetlanaNeural".to_owned())
            } else {
                None
            }
        });
        let ids = index
            .get("books")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| Error::Format("В index.json отсутствует список книг".into()))?;
        let mut books = Vec::new();
        for id in ids {
            let id = id
                .as_str()
                .ok_or_else(|| Error::Format("Некорректный идентификатор книги".into()))?;
            if !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return Err(Error::Format("Некорректный идентификатор книги".into()));
            }
            let bytes = read(&root.join(format!("{id}.json")), 100 * 1024 * 1024)?;
            let mut book: Book = serde_json::from_slice(&bytes)?;
            if book.id != id || book.chapters.is_empty() {
                return Err(Error::Format(
                    "Некорректная книга в старой библиотеке".into(),
                ));
            }
            book.marks.retain(|m| m.chapter < book.chapters.len());
            for mark in &mut book.marks {
                mark.id = Uuid::new_v4().to_string();
                mark.offset = mark
                    .offset
                    .min(book.chapters[mark.chapter].text.encode_utf16().count());
            }
            if !book.speed.is_finite() || !(0.25..=4.0).contains(&book.speed) {
                book.speed = 1.0;
            }
            book.refresh();
            books.push(book);
        }
        let copied = if let Some(path) = prefs.background_image_path.as_deref() {
            let original = Path::new(path);
            let source = if original.is_file() {
                original.to_path_buf()
            } else {
                root.join(original.file_name().unwrap_or_default())
            };
            if source.is_file() {
                let ext = source
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if ["png", "jpg", "jpeg", "webp", "gif", "bmp"].contains(&ext.as_str()) {
                    let target = self
                        .root
                        .join(format!("background-{}.{}", Uuid::new_v4(), ext));
                    fs::copy(source, &target)?;
                    Some(target)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        prefs.background_image_path = copied.as_ref().map(|p| p.to_string_lossy().into_owned());
        let result = (|| {
            let tx = self.conn.unchecked_transaction()?;
            let mut count = 0;
            for book in &books {
                if Self::insert(&tx, book)? {
                    count += 1;
                }
            }
            tx.execute("INSERT INTO settings (key,data) VALUES ('preferences',?1) ON CONFLICT(key) DO UPDATE SET data=excluded.data", [serde_json::to_string(&prefs)?])?;
            tx.commit()?;
            Ok(count)
        })();
        if result.is_err()
            && let Some(path) = copied
        {
            let _ = fs::remove_file(path);
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn migrates_backups_and_clamps_positions() -> std::result::Result<(), Box<dyn std::error::Error>>
    {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let legacy = dir.join("legacy");
        fs::create_dir_all(&legacy)?;
        fs::write(
            legacy.join("index.json.bak"),
            r#"{"books":["1"],"prefs":{"fontSize":200,"voice":"Edge Дмитрий · онлайн|ru-RU"}}"#,
        )?;
        fs::write(
            legacy.join("1.json.bak"),
            r#"{"id":"1","title":"Книга","chapters":[{"title":"Глава","text":"😀"}],"chapter":8,"offset":900,"marks":[{"id":"x","chapter":0,"offset":900,"excerpt":""}]}"#,
        )?;
        let store = Store::open(&dir.join("new"))?;
        assert_eq!(store.import_legacy(&legacy)?, 1);
        assert_eq!(store.import_legacy(&legacy)?, 0);
        let book = store.book("1")?;
        assert_eq!((book.chapter, book.offset, book.marks[0].offset), (0, 2, 2));
        let prefs = store.preferences()?;
        assert_eq!(prefs.font_size, 60.0);
        assert_eq!(prefs.voice.as_deref(), Some("ru-RU-DmitryNeural"));
        drop(store);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn validates_and_persists_volume() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        let store = Store::open(&dir)?;
        let mut prefs = Preferences {
            volume: 0.35,
            ..Preferences::default()
        };
        store.save_preferences(&prefs)?;
        for invalid in [-0.01, 1.01, f64::NAN, f64::INFINITY] {
            prefs.volume = invalid;
            assert!(store.save_preferences(&prefs).is_err());
        }
        drop(store);
        let store = Store::open(&dir)?;
        assert_eq!(store.preferences()?.volume, 0.35);
        store.conn.execute(
            "UPDATE settings SET data=?1 WHERE key='preferences'",
            [r#"{"volume":2}"#],
        )?;
        assert_eq!(store.preferences()?.volume, 1.0);
        drop(store);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
    #[test]
    fn persists_position_and_bookmark() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(Uuid::new_v4().to_string());
        fs::create_dir_all(&dir)?;
        let source = dir.join("book.txt");
        fs::write(&source, "😀 Привет. Мир!")?;
        let store = Store::open(&dir)?;
        let summary = store
            .import(&[source.to_string_lossy().into_owned()])?
            .remove(0);
        store.position(&summary.id, 0, 3, 1.2)?;
        let mark = store.add_mark(&summary.id, 0, 3)?;
        drop(store);
        let store = Store::open(&dir)?;
        let book = store.book(&summary.id)?;
        assert_eq!(book.offset, 3);
        assert_eq!(book.marks[0].id, mark.id);
        drop(store);
        fs::remove_dir_all(dir)?;
        Ok(())
    }
}
