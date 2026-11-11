"""Download Wikipedia articles from Hugging Face into newline-delimited JSON."""

import argparse
import json
from pathlib import Path
from urllib.error import URLError
from urllib.parse import urlencode
from urllib.request import urlopen


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--config", default="20231101.en",
        help="Wikipedia snapshot/language, e.g. 20231101.en",
    )
    parser.add_argument("--output", type=Path, default=Path("wikipedia.jsonl"))
    parser.add_argument("--limit", type=int, help="maximum number of articles to save")
    args = parser.parse_args()
    if args.limit is not None and args.limit < 1:
        parser.error("--limit must be at least 1")

    args.output.parent.mkdir(parents=True, exist_ok=True)
    count = 0
    with args.output.open("w", encoding="utf-8") as output:
        while args.limit is None or count < args.limit:
            query = urlencode({
                "dataset": "wikimedia/wikipedia",
                "config": args.config,
                "split": "train",
                "offset": count,
                "length": 100 if args.limit is None else min(100, args.limit - count),
            })
            try:
                with urlopen(
                    f"https://datasets-server.huggingface.co/rows?{query}", timeout=60
                ) as response:
                    page = json.load(response)
            except (URLError, TimeoutError) as error:
                raise SystemExit(
                    f"Download failed after {count:,} articles: {error}"
                ) from error

            for item in page["rows"]:
                article = item["row"]
                record = {
                    "id": str(article.get("id", count)),
                    "title": article.get("title", ""),
                    "text": article.get("text", ""),
                }
                output.write(json.dumps(record, ensure_ascii=False) + "\n")
                count += 1
            print(f"Saved {count:,} articles to {args.output}", flush=True)
            if not page["rows"] or count >= page["num_rows_total"]:
                break


if __name__ == "__main__":
    main()
