use rust_stemmers::{Algorithm, Stemmer};
use std::{borrow::Cow, sync::LazyLock};
use unicode_segmentation::UnicodeSegmentation;

static STEMMER: LazyLock<Stemmer> = LazyLock::new(|| Stemmer::create(Algorithm::English));

pub fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    UnicodeSegmentation::unicode_words(text).map(|word| {
        let lowercase = word.to_lowercase();
        match STEMMER.stem(&lowercase) {
            Cow::Borrowed(_) => lowercase,
            Cow::Owned(stem) => stem,
        }
    })
}
