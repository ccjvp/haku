#!/usr/bin/env python3
"""Post JSONL Wikipedia articles to a running Haku server."""

import argparse
import json
import time
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import Request, urlopen


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("input", help="JSONL file produced by download_wikipedia.py")
    parser.add_argument(
        "--server", default="http://127.0.0.1:3000", help="Haku server base URL"
    )
    parser.add_argument("--limit", type=int, help="maximum number of articles to post")
    parser.add_argument("--retries", type=int, default=3)
    parser.add_argument(
        "--pause",
        type=float,
        default=0.0,
        help="seconds to wait between successful posts",
    )
    parser.add_argument(
        "--max-bytes",
        type=int,
        default=8 * 1024 * 1024,
        help="skip articles larger than this (match the server's --max-chunk-size)",
    )
    args = parser.parse_args()
    base_url = args.server.rstrip("/")

    sent = 0
    with open(args.input, encoding="utf-8") as source:
        for line_number, line in enumerate(source, 1):
            if args.limit is not None and sent >= args.limit:
                break
            if not line.strip():
                continue
            try:
                article = json.loads(line)
                name = str(article["id"])
                text = f"{article.get('title', '')}\n\n{article['text']}"
            except (json.JSONDecodeError, KeyError, TypeError) as error:
                raise SystemExit(
                    f"Invalid article on line {line_number}: {error}"
                ) from error

            # The server stores each POST body as a document and defaults to an 8 MiB limit.
            body = text.encode("utf-8")
            if len(body) > args.max_bytes:
                print(
                    f"Skipping {name}: article exceeds --max-bytes ({args.max_bytes:,})",
                    flush=True,
                )
                continue
            request = Request(
                f"{base_url}/{quote(name, safe='')}",
                data=body,
                headers={"Content-Type": "text/plain; charset=utf-8"},
                method="POST",
            )

            for attempt in range(args.retries + 1):
                try:
                    with urlopen(request, timeout=60) as response:
                        if response.status not in (200, 201, 204):
                            raise RuntimeError(
                                f"unexpected HTTP status {response.status}"
                            )
                    break
                except HTTPError as error:
                    detail = error.read().decode("utf-8", errors="replace")
                    message = f"HTTP {error.code} for article {name}: {detail}"
                    if error.code < 500 or attempt == args.retries:
                        raise SystemExit(message) from error
                except (URLError, TimeoutError) as error:
                    message = f"Request failed for article {name}: {error}"
                    if attempt == args.retries:
                        raise SystemExit(message) from error
                time.sleep(min(2**attempt, 30))

            sent += 1
            if sent % 1000 == 0:
                print(f"Posted {sent:,} articles to {base_url}", flush=True)
            if args.pause:
                time.sleep(args.pause)


if __name__ == "__main__":
    main()
