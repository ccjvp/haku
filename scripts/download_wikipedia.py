#!/usr/bin/env python3
"""Stream a Hugging Face Wikipedia snapshot into newline-delimited JSON."""

import argparse
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--config",
        default="20231101.en",
        help="Wikipedia snapshot/language, e.g. 20231101.en",
    )
    parser.add_argument("--output", type=Path, default=Path("wikipedia.jsonl"))
    parser.add_argument(
        "--limit", type=int, help="maximum number of articles to save (for a trial run)"
    )
    args = parser.parse_args()

    try:
        from datasets import load_dataset
    except ImportError as error:
        raise SystemExit(
            "Install the downloader dependency with: python -m pip install datasets"
        ) from error

    articles = load_dataset(
        "wikimedia/wikipedia", args.config, split="train", streaming=True
    )
    args.output.parent.mkdir(parents=True, exist_ok=True)
    count = 0
    with args.output.open("w", encoding="utf-8") as output:
        for article in articles:
            record = {
                "id": str(article.get("id", count)),
                "title": article.get("title", ""),
                "text": article.get("text", ""),
            }
            output.write(json.dumps(record, ensure_ascii=False) + "\n")
            count += 1
            if count % 10000 == 0:
                print(f"Saved {count:,} articles", flush=True)
            if args.limit is not None and count >= args.limit:
                break

    print(f"Saved {count:,} articles to {args.output}")


if __name__ == "__main__":
    main()
