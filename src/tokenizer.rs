use rust_stemmers::{Algorithm, Stemmer};
use std::{borrow::Cow, collections::HashMap, sync::LazyLock};
use unicode_segmentation::UnicodeSegmentation;

static STEMMER: LazyLock<Stemmer> = LazyLock::new(|| Stemmer::create(Algorithm::English));

pub fn tokenize(text: &str) -> HashMap<String, u32> {
    let mut map = HashMap::new();

    UnicodeSegmentation::unicode_words(text).for_each(|word| {
        let lowercase = word.to_lowercase();

        match STEMMER.stem(&lowercase) {
            Cow::Borrowed(_) => {
                *map.entry(lowercase).or_insert(0) += 1;
            }
            Cow::Owned(stem) => {
                *map.entry(stem).or_insert(0) += 1;
            }
        }
    });

    map
}
