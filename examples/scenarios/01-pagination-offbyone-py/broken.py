"""Paginated search over a flat list of records.

The bug: `paginate(items, page, per_page)` is supposed to return
`per_page` consecutive items, but every page after the first
silently drops or duplicates a record. The driver below asserts
that walking every page reconstructs the original list — that
assertion fails on the second page.
"""

from dataclasses import dataclass


@dataclass
class Record:
    id: int
    name: str


def make_corpus(n: int) -> list[Record]:
    return [Record(id=i, name=f"row-{i:03d}") for i in range(n)]


def paginate(items: list[Record], page: int, per_page: int) -> list[Record]:
    # `page` is 1-indexed in the public API.
    start = (page - 1) * per_page
    end = start + per_page   # ← inclusive end? off-by-one lives somewhere here
    return items[start:end]


def walk_all_pages(items: list[Record], per_page: int) -> list[Record]:
    out: list[Record] = []
    page = 1
    while True:
        chunk = paginate(items, page, per_page)
        if not chunk:
            break
        out.extend(chunk)
        page += 1
        if page > 100:   # guard against infinite loops while debugging
            raise RuntimeError("page limit exceeded — pagination is wrong")
    return out


def main() -> None:
    corpus = make_corpus(23)
    seen = walk_all_pages(corpus, per_page=5)
    expected_ids = [r.id for r in corpus]
    seen_ids = [r.id for r in seen]
    if seen_ids != expected_ids:
        # Print enough context to be useful from a stack trace.
        dup = [i for i in seen_ids if seen_ids.count(i) > 1]
        miss = [i for i in expected_ids if i not in seen_ids]
        raise AssertionError(
            f"pagination drift: duplicated={sorted(set(dup))} missing={miss}"
        )
    print(f"OK: walked {len(seen)} records across pages of 5")


if __name__ == "__main__":
    main()
