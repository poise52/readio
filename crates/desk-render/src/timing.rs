use msedge_tts::tts::AudioMetadata;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SpeechTiming {
    pub start_utf16: usize,
    pub end_utf16: usize,
    pub start_seconds: f64,
    pub end_seconds: f64,
}

const fn separator(c: char) -> bool {
    c.is_whitespace()
        || c.is_ascii_punctuation()
        || matches!(
            c,
            '«' | '»' | '„' | '“' | '”' | '‘' | '’' | '—' | '–' | '…' | '‐' | '‑' | '−'
        )
}

pub fn collect_edge_timings(text: &str, metadata: &[AudioMetadata]) -> Option<Vec<SpeechTiming>> {
    let mut marks = Vec::new();
    let mut cursor = 0;
    let mut previous_end = 0;
    for item in metadata
        .iter()
        .filter(|item| item.metadata_type.as_deref() == Some("WordBoundary"))
    {
        let word = item.text.as_deref().filter(|word| !word.is_empty())?;
        let relative = text.get(cursor..)?.find(word)?;
        let start = cursor.checked_add(relative)?;
        if !text.get(cursor..start)?.chars().all(separator)
            || item.duration == 0
            || item.offset < previous_end
        {
            return None;
        }
        let end = start.checked_add(word.len())?;
        let ticks_end = item.offset.checked_add(item.duration)?;
        let seconds = |ticks: u64| -> Option<f64> {
            let whole = u32::try_from(ticks / 10_000_000).ok()?;
            let fraction = u32::try_from(ticks % 10_000_000).ok()?;
            Some(f64::from(whole) + f64::from(fraction) / 10_000_000.0)
        };
        marks.push(SpeechTiming {
            start_utf16: text.get(..start)?.encode_utf16().count(),
            end_utf16: text.get(..end)?.encode_utf16().count(),
            start_seconds: seconds(item.offset)?,
            end_seconds: seconds(ticks_end)?,
        });
        cursor = end;
        previous_end = ticks_end;
    }
    if marks.is_empty() || !text.get(cursor..)?.chars().all(separator) {
        return None;
    }
    Some(marks)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn word(text: &str, offset: u64, duration: u64) -> AudioMetadata {
        AudioMetadata {
            metadata_type: Some("WordBoundary".into()),
            offset,
            duration,
            text: Some(text.into()),
            length: text.encode_utf16().count() as u64,
            boundary_type: Some("Word".into()),
        }
    }
    #[test]
    fn repeated_words_punctuation_and_utf16_are_ordered() {
        let marks = collect_edge_timings(
            "Привет, привет! 😀",
            &[
                word("Привет", 1_000_000, 2_000_000),
                word("привет", 4_000_000, 1_000_000),
                word("😀", 6_000_000, 1_000_000),
            ],
        )
        .unwrap();
        assert_eq!(
            marks[0],
            SpeechTiming {
                start_utf16: 0,
                end_utf16: 6,
                start_seconds: 0.1,
                end_seconds: 0.3
            }
        );
        assert_eq!((marks[2].start_utf16, marks[2].end_utf16), (16, 18));
        assert_eq!(marks[2].start_seconds, 0.6);
        assert_eq!(
            serde_json::to_value(&marks[0]).unwrap(),
            serde_json::json!({"startUtf16": 0, "endUtf16": 6, "startSeconds": 0.1, "endSeconds": 0.3})
        );
    }
    #[test]
    fn omitted_or_changed_words_never_get_guessed() {
        assert!(
            collect_edge_timings("раз два раз", &[word("раз", 0, 1), word("раз", 2, 1)]).is_none()
        );
        assert!(
            collect_edge_timings("один 😀 два", &[word("один", 0, 1), word("два", 2, 1)]).is_none()
        );
        assert!(collect_edge_timings("раз два", &[word("раз", 1, 2), word("два", 2, 2)]).is_none());
    }
}
