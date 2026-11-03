// SPDX-License-Identifier: Apache-2.0
// Vendored from https://github.com/iwe-org/iwe/blob/7cdf04cb02b7b80c234e8403fddd2b0d1e1cc5e8/crates/iwe/benches/common.rs
// See LICENSE in this directory.

const CATEGORIES: &[&str] = &["alpha", "beta", "gamma", "delta"];
const STATUSES: &[&str] = &["draft", "published", "scheduled"];

const INCLUSION_LINKS_PER_DOC: usize = 3;
const SECTIONS_PER_DOC: usize = 2;
const PARAGRAPHS_PER_SECTION: usize = 3;

pub fn doc_key(idx: usize) -> String {
    format!("doc-{:05}", idx)
}

pub fn hub_key() -> &'static str {
    "hub"
}

pub fn generate_corpus(dir: &std::path::Path, n_docs: usize, seed: u64) {
    let mut rng = <rand::rngs::StdRng as rand::SeedableRng>::seed_from_u64(seed);

    for i in 1..=n_docs {
        let key = doc_key(i);
        let category = CATEGORIES[rand::Rng::random_range(&mut rng, 0..CATEGORIES.len())];
        let status = STATUSES[rand::Rng::random_range(&mut rng, 0..STATUSES.len())];

        let mut content = String::new();
        content.push_str("---\n");
        content.push_str(&format!("title: \"Doc {:05}\"\n", i));
        content.push_str(&format!("type: post\n"));
        content.push_str(&format!("category: {}\n", category));
        content.push_str(&format!("status: {}\n", status));
        content.push_str("created: 2026-01-01\n");
        content.push_str("---\n\n");

        for _ in 0..INCLUSION_LINKS_PER_DOC {
            let target = pick_other(&mut rng, n_docs, i);
            let target_key = doc_key(target);
            content.push_str(&format!("[Doc {:05}]({})\n\n", target, target_key));
        }

        for s in 0..SECTIONS_PER_DOC {
            content.push_str(&format!("## Section {}\n\n", s + 1));
            for _ in 0..PARAGRAPHS_PER_SECTION {
                let target = pick_other(&mut rng, n_docs, i);
                let target_key = doc_key(target);
                content.push_str(&format!(
                    "Some paragraph text mentioning [Doc {:05}]({}) in passing. \
                     Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n\n",
                    target, target_key
                ));
            }
        }

        std::fs::write(dir.join(format!("{}.md", key)), content).expect("write doc");
    }

    let mut hub = String::new();
    hub.push_str("---\n");
    hub.push_str("title: \"Hub\"\n");
    hub.push_str("type: hub\n");
    hub.push_str("category: alpha\n");
    hub.push_str("status: published\n");
    hub.push_str("created: 2026-01-01\n");
    hub.push_str("---\n\n");
    let hub_targets = (n_docs / 10).max(1);
    for _ in 0..hub_targets {
        let target = 1 + rand::Rng::random_range(&mut rng, 0..n_docs);
        let target_key = doc_key(target);
        hub.push_str(&format!("[Doc {:05}]({})\n\n", target, target_key));
    }
    std::fs::write(dir.join(format!("{}.md", hub_key())), hub).expect("write hub");
}

fn pick_other(mut rng: &mut rand::rngs::StdRng, n_docs: usize, exclude: usize) -> usize {
    if n_docs <= 1 {
        return exclude;
    }
    loop {
        let pick = 1 + rand::Rng::random_range(&mut rng, 0..n_docs);
        if pick != exclude {
            return pick;
        }
    }
}
