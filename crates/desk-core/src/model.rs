use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Chapter {
    pub title: String,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Mark {
    pub id: String,
    pub chapter: usize,
    pub offset: usize,
    pub excerpt: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct Passage {
    pub start: usize,
    pub end: usize,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Book {
    pub id: String,
    pub title: String,
    pub author: String,
    pub cover: Option<String>,
    pub chapters: Vec<Chapter>,
    pub chapter: usize,
    pub offset: usize,
    pub speed: f32,
    #[serde(default, rename = "audioPosition")]
    pub audio_position: Option<AudioPosition>,
    pub marks: Vec<Mark>,
    #[serde(default, skip_deserializing)]
    pub passages: Vec<Passage>,
    #[serde(default, skip_deserializing)]
    pub progress: f64,
}
impl Default for Book {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            author: String::new(),
            cover: None,
            chapters: Vec::new(),
            chapter: 0,
            offset: 0,
            speed: 1.0,
            audio_position: None,
            marks: Vec::new(),
            passages: Vec::new(),
            progress: 0.0,
        }
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct BookSummary {
    pub id: String,
    pub title: String,
    pub author: String,
    pub cover: Option<String>,
    pub chapter: usize,
    pub offset: usize,
    pub speed: f32,
    pub progress: f64,
}
#[derive(Clone, Debug, Serialize)]
pub struct ChapterSummary {
    pub title: String,
    pub length: usize,
}
#[derive(Clone, Debug, Serialize)]
pub struct ReaderBook {
    pub id: String,
    pub title: String,
    pub author: String,
    pub cover: Option<String>,
    pub chapters: Vec<ChapterSummary>,
    pub chapter: usize,
    pub offset: usize,
    pub speed: f32,
    #[serde(rename = "audioPosition")]
    pub audio_position: Option<AudioPosition>,
    pub marks: Vec<Mark>,
    pub passages: Vec<Passage>,
    pub progress: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AudioPosition {
    pub chapter: usize,
    pub block_id: String,
    pub profile_id: String,
    pub seconds: f64,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookmarkEntry {
    pub book: BookSummary,
    pub mark: Mark,
    pub chapter_title: String,
}
impl Book {
    pub fn refresh(&mut self) {
        self.chapter = self.chapter.min(self.chapters.len().saturating_sub(1));
        let total: usize = self
            .chapters
            .iter()
            .map(|c| c.text.encode_utf16().count())
            .sum();
        let done: usize = self
            .chapters
            .iter()
            .take(self.chapter)
            .map(|c| c.text.encode_utf16().count())
            .sum();
        if let Some(chapter) = self.chapters.get(self.chapter) {
            self.offset = self.offset.min(chapter.text.encode_utf16().count());
            self.passages = split_passages(&chapter.text);
        } else {
            self.offset = 0;
            self.passages.clear();
        }
        self.progress = if total == 0 {
            0.0
        } else {
            f64::from(u32::try_from(done + self.offset).unwrap_or(u32::MAX))
                / f64::from(u32::try_from(total).unwrap_or(u32::MAX))
        };
    }
    #[must_use]
    pub fn summary(&self) -> BookSummary {
        BookSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            author: self.author.clone(),
            cover: self.cover.clone(),
            chapter: self.chapter,
            offset: self.offset,
            speed: self.speed,
            progress: self.progress,
        }
    }
}
#[must_use]
pub fn split_passages(input: &str) -> Vec<Passage> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut utf16 = 0;
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (byte, ch) = chars[i];
        if matches!(ch, '.' | '!' | '?' | '…' | '\n') {
            let end_byte = if ch == '\n' {
                byte
            } else {
                let mut j = i + 1;
                while j < chars.len()
                    && matches!(
                        chars[j].1,
                        '.' | '!' | '?' | '…' | '”' | '»' | '"' | '\'' | ')'
                    )
                {
                    j += 1;
                }
                i = j - 1;
                chars.get(j).map_or(input.len(), |(b, _)| *b)
            };
            push_passage(&mut result, &input[start..end_byte], utf16);
            let next = if ch == '\n' { byte + 1 } else { end_byte };
            utf16 += input[start..next].encode_utf16().count();
            start = next;
        }
        i += 1;
    }
    push_passage(&mut result, &input[start..], utf16);
    result
}
fn push_passage(result: &mut Vec<Passage>, raw: &str, offset: usize) {
    let text = raw.trim();
    if !text.is_empty() {
        let leading = raw.len() - raw.trim_start().len();
        let start = offset + raw[..leading].encode_utf16().count();
        result.push(Passage {
            start,
            end: start + text.encode_utf16().count(),
            text: text.to_owned(),
        });
    }
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BillingUnit {
    Character,
    Utf8Byte,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Preferences {
    pub dark: bool,
    pub font_size: f64,
    pub line_height: f64,
    pub primary_color: u32,
    pub background_image_path: Option<String>,
    pub background_opacity: f64,
    pub background_dim: f64,
    pub background_blur_enabled: bool,
    pub background_blur: f64,
    pub surface_transparency: f64,
    pub voice: Option<String>,
    pub open_router_model: Option<String>,
    pub open_router_voice: Option<String>,
    pub open_router_billing_units: BTreeMap<String, BillingUnit>,
    #[serde(default)]
    pub open_router_model_settings: BTreeMap<String, crate::SynthesisSettings>,
    #[serde(default = "default_volume")]
    pub volume: f64,
    pub dictionary: BTreeMap<String, String>,
}
#[must_use]
const fn default_volume() -> f64 {
    1.0
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            dark: true,
            font_size: 19.0,
            line_height: 1.65,
            primary_color: 0xff69_d9b1,
            background_image_path: None,
            background_opacity: 0.24,
            background_dim: 0.20,
            background_blur_enabled: false,
            background_blur: 0.4,
            surface_transparency: 0.18,
            voice: None,
            open_router_model: None,
            open_router_voice: None,
            open_router_billing_units: BTreeMap::new(),
            open_router_model_settings: BTreeMap::new(),
            volume: default_volume(),
            dictionary: BTreeMap::new(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_preferences_default_openrouter_voice() -> Result<(), serde_json::Error> {
        let prefs: Preferences = serde_json::from_str(r#"{"dark":false}"#)?;
        assert_eq!(prefs.open_router_model, None);
        assert_eq!(prefs.open_router_voice, None);
        assert!(
            serde_json::to_value(&prefs)?
                .get("openRouterVoice")
                .is_some()
        );
        assert_eq!(prefs.volume, 1.0);
        assert_eq!(prefs.open_router_model_settings, BTreeMap::new());
        assert_eq!(
            serde_json::to_value(&prefs)?["openRouterModelSettings"],
            serde_json::json!({})
        );
        assert_eq!(
            serde_json::to_value(Preferences::default())?["openRouterModelSettings"],
            serde_json::json!({})
        );
        Ok(())
    }
    #[test]
    fn utf16_offsets_include_surrogate_pairs() {
        let passages = split_passages("  😀 Привет!  Мир.\nДа");
        assert_eq!(
            passages
                .iter()
                .map(|p| (p.start, p.end))
                .collect::<Vec<_>>(),
            vec![(2, 12), (14, 18), (19, 21)]
        );
    }
}
