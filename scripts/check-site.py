#!/usr/bin/env python3
"""Dependency-free structural and asset smoke checks for the Pages site."""
from __future__ import annotations

import re
import sys
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
SITE = ROOT / "site"
BASE = "/caravan/"
REQUIRED_META = {"description", "og:title", "og:description", "og:image", "og:url"}


class PageParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.ids: list[str] = []
        self.refs: list[tuple[str, str, str]] = []
        self.meta: set[str] = set()
        self.title = False
        self.canonical = False

    def handle_starttag(self, tag: str, attrs_list: list[tuple[str, str | None]]) -> None:
        attrs = dict(attrs_list)
        if value := attrs.get("id"):
            self.ids.append(value)
        for attribute in ("href", "src"):
            if value := attrs.get(attribute):
                self.refs.append((tag, attribute, value))
        if tag == "title":
            self.title = True
        if tag == "meta":
            key = attrs.get("name") or attrs.get("property")
            if key:
                self.meta.add(key)
        if tag == "link" and attrs.get("rel") == "canonical":
            self.canonical = attrs.get("href") == "https://a.skh.am/caravan/"


def fail(message: str) -> None:
    print(f"site check: {message}", file=sys.stderr)
    raise SystemExit(1)


def local_path(reference: str) -> Path | None:
    parts = urlsplit(reference)
    if parts.scheme or parts.netloc or reference.startswith("//"):
        return None
    if parts.path.startswith("/"):
        if not parts.path.startswith(BASE):
            fail(f"project-local URL must begin with {BASE}: {reference}")
        relative = unquote(parts.path[len(BASE):])
    else:
        relative = unquote(parts.path)
    return SITE / (relative or "index.html")


def main() -> None:
    index = SITE / "index.html"
    if not index.is_file():
        fail("site/index.html is missing")
    parser = PageParser()
    parser.feed(index.read_text(encoding="utf-8"))

    duplicates = sorted({item for item in parser.ids if parser.ids.count(item) > 1})
    if duplicates:
        fail(f"duplicate HTML ids: {', '.join(duplicates)}")
    if not parser.title or not parser.canonical:
        fail("title or canonical project URL is missing")
    missing_meta = REQUIRED_META - parser.meta
    if missing_meta:
        fail(f"missing metadata: {', '.join(sorted(missing_meta))}")

    ids = set(parser.ids)
    checked: set[Path] = set()
    for tag, attribute, reference in parser.refs:
        if reference.startswith(("mailto:", "tel:")):
            continue
        parts = urlsplit(reference)
        if not parts.path and parts.fragment:
            if parts.fragment not in ids:
                fail(f"broken fragment #{parts.fragment}")
            continue
        path = local_path(reference)
        if path is None:
            if tag in {"script", "img", "iframe", "audio", "video", "source"} or (tag == "link" and reference != "https://a.skh.am/caravan/"):
                fail(f"externally hosted page asset is not allowed: {reference}")
            continue
        if not path.exists():
            fail(f"missing local asset for {reference}: {path.relative_to(ROOT)}")
        checked.add(path)
        if parts.fragment and path == index and parts.fragment not in ids:
            fail(f"broken fragment #{parts.fragment}")

    css = (SITE / "assets/styles.css").read_text(encoding="utf-8")
    if re.search(r"@import|url\(\s*['\"]?https?://", css, re.IGNORECASE):
        fail("CSS imports an external resource")
    if not (SITE / ".nojekyll").exists():
        fail("site/.nojekyll is missing")
    print(f"site check: ok ({len(parser.refs)} references, {len(checked)} local targets, {len(ids)} ids)")


if __name__ == "__main__":
    main()
