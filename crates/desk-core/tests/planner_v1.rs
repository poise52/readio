use desk_core::{
    AudioProfile, Book, Chapter, ChapterPlanner, Preferences, plan_chapter, plan_chapter_v1,
};
use serde_json::json;

#[test]
fn v1_preserves_canonical_ids_text_and_utf16_offsets() {
    let mut profile = AudioProfile::from_preferences(&Preferences::default());
    profile.dictionary.insert("мир".into(), "МИИР".into());
    profile.dictionary.insert("hello".into(), "привет".into());
    profile.max_escaped_bytes = Some(42);
    let book = Book {
        chapters: vec![Chapter {
            title: "Тест".into(),
            text: "😀 Мир, мирный! HELLO <мир>? Ёж и мир.\nHello, мир!".into(),
        }],
        ..Book::default()
    };
    let expected = json!([
      {"id":"28d46dad4faba956c02b21a22e5bbdab7e09525eb9d798f0d40fb177b96dfd77","chapter":0,"start":0,"end":21,"passages":[{"index":0,"start":0,"end":15,"weight":36},{"index":1,"start":16,"end":21,"weight":6}],"text":"😀 МИИР, мирный!\n\nпривет","characters":23},
      {"id":"a4c100e7abb5ed095b39ac755ef8f1e20444e62edd05166e49d652340392f624","chapter":0,"start":21,"end":44,"passages":[{"index":1,"start":21,"end":28,"weight":22},{"index":2,"start":29,"end":38,"weight":24},{"index":3,"start":39,"end":44,"weight":1}],"text":" <МИИР>?\n\nЁж и МИИР.\n\nп","characters":23},
      {"id":"32b635c77ccbaa94091b302ed420cf6455b673441714a80115b97eb57860e80e","chapter":0,"start":39,"end":50,"passages":[{"index":3,"start":39,"end":50,"weight":32}],"text":"ривет, МИИР!","characters":12}
    ]);
    let planner = ChapterPlanner::new(&profile);
    for actual in [
        planner.plan_chapter(0, &book.chapters[0]),
        planner.plan_text(0, &book.chapters[0].text),
        planner.plan_book(&book),
        plan_chapter(&book, 0, &profile),
        plan_chapter_v1(&book, 0, &profile),
    ] {
        assert_eq!(serde_json::to_value(actual).unwrap(), expected);
    }
    assert!(plan_chapter_v1(&book, 1, &profile).is_empty());
}
