#!/usr/bin/env python3
"""Write the sample data folders alkyon's folder and file sources are tested on.

    python docker/seed/make-sample-files.py

Everything lands under `sample-data/` at the repository root, which is gitignored:
these are fixtures to click around in, not artefacts to commit. Re-running is safe
and overwrites.

Deliberately dependency-free for CSV and JSON — parquet needs `pyarrow` and Excel
needs `openpyxl`, and each is skipped with a message rather than failing the run, so
you get whatever your interpreter can produce.
"""

from __future__ import annotations

import csv
import datetime as dt
import json
import pathlib
import random
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2] / "sample-data"

COUNTRIES = ["BE", "FR", "NL", "DE", None]
TIERS = ["gold", "silver", "bronze"]
SKUS = [f"SKU-{n:04d}" for n in range(1, 40)]


def customers(count: int) -> list[dict]:
    random.seed(11)
    start = dt.date(2022, 1, 1)
    return [
        {
            "id": n,
            "name": f"Customer {n}",
            "country": COUNTRIES[n % len(COUNTRIES)],
            "tier": TIERS[n % len(TIERS)],
            "credit": round(n * 13.37, 2),
            "signed_on": (start + dt.timedelta(days=n % 400)).isoformat(),
        }
        for n in range(1, count + 1)
    ]


def orders(count: int, customer_count: int) -> list[dict]:
    random.seed(23)
    start = dt.datetime(2022, 1, 1, 8, 0, 0)
    return [
        {
            "order_id": 1000 + n,
            "customer_id": 1 + (n % customer_count),
            "sku": SKUS[n % len(SKUS)],
            "quantity": 1 + (n % 7),
            "unit_price": round(9.99 + (n % 40) + n % 3 / 4, 2),
            "ordered_at": (start + dt.timedelta(hours=n * 7)).isoformat(sep=" "),
            "shipped": None if n % 4 == 0 else "yes",
        }
        for n in range(count)
    ]


def write_csv(path: pathlib.Path, rows: list[dict], **kwargs) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    # `newline=""` is what stops Python writing \r\r\n on Windows — and a CSV with
    # mixed line endings is exactly what DuckDB's sniffer refuses.
    # Two separate traps, and DuckDB's sniffer refuses the result of either:
    # `newline=""` stops Python translating on top of what csv writes, and
    # `lineterminator` stops csv writing CRLF in the first place. Without the
    # second, a file whose preamble is LF and whose rows are CRLF has *mixed* line
    # endings — which is the fixture bug this comment is paying for.
    with path.open("w", newline="", encoding=kwargs.pop("encoding", "utf-8")) as handle:
        writer = csv.DictWriter(
            handle, fieldnames=list(rows[0]), lineterminator="\n", **kwargs
        )
        writer.writeheader()
        writer.writerows(rows)
    print(f"  {path.relative_to(ROOT)}  ({len(rows)} rows)")


def write_european_csv(path: pathlib.Path, rows: list[dict]) -> None:
    """Semicolons, comma decimals, Latin-1, and a preamble above the header.

    The point of this one is to be *wrong* for every default: it is what the
    CSV options in the source dialogue exist for.
    """
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="", encoding="latin-1") as handle:
        # Accented, so the encoding actually matters: read as UTF-8 these become
        # replacement characters, which is the symptom the option exists to cure.
        handle.write("Export du 1er février\n")
        handle.write("Généré automatiquement - ne pas éditer\n")
        writer = csv.DictWriter(
            handle, fieldnames=list(rows[0]), delimiter=";", lineterminator="\n"
        )
        writer.writeheader()
        for row in rows:
            writer.writerow(
                {
                    key: (str(value).replace(".", ",") if isinstance(value, float) else value)
                    for key, value in row.items()
                }
            )
    print(f"  {path.relative_to(ROOT)}  ({len(rows)} rows, ; and , and latin-1)")


def write_json(path: pathlib.Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(rows, indent=1), encoding="utf-8")
    print(f"  {path.relative_to(ROOT)}  ({len(rows)} rows)")


def write_jsonl(path: pathlib.Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8", newline="\n") as handle:
        for row in rows:
            handle.write(json.dumps(row) + "\n")
    print(f"  {path.relative_to(ROOT)}  ({len(rows)} rows)")


def write_parquet(path: pathlib.Path, rows: list[dict]) -> bool:
    try:
        import pyarrow as pa
        import pyarrow.parquet as pq
    except ImportError:
        return False
    path.parent.mkdir(parents=True, exist_ok=True)
    table = pa.Table.from_pylist(rows)
    pq.write_table(table, path)
    print(f"  {path.relative_to(ROOT)}  ({len(rows)} rows)")
    return True


def write_excel(path: pathlib.Path, sheets: dict[str, list[dict]]) -> bool:
    try:
        from openpyxl import Workbook
    except ImportError:
        return False
    path.parent.mkdir(parents=True, exist_ok=True)
    book = Workbook()
    book.remove(book.active)
    for name, rows in sheets.items():
        sheet = book.create_sheet(name)
        sheet.append(list(rows[0]))
        for row in rows:
            sheet.append(list(row.values()))
    book.save(path)
    print(f"  {path.relative_to(ROOT)}  ({', '.join(sheets)})")
    return True


def main() -> int:
    people = customers(250)
    lines = orders(1500, len(people))

    print(f"writing {ROOT}")

    print("csv/")
    write_csv(ROOT / "csv" / "customers.csv", people)
    write_csv(ROOT / "csv" / "sales" / "orders.csv", lines)
    write_european_csv(ROOT / "csv-european" / "ventes.csv", people)

    print("json/")
    write_json(ROOT / "json" / "customers.json", people)
    write_json(ROOT / "json" / "sales" / "orders.json", lines)

    print("jsonl/")
    write_jsonl(ROOT / "jsonl" / "customers.jsonl", people)
    write_jsonl(ROOT / "jsonl" / "orders.ndjson", lines)

    print("parquet/")
    if not write_parquet(ROOT / "parquet" / "customers.parquet", people):
        print("  skipped — pip install pyarrow")
    else:
        write_parquet(ROOT / "parquet" / "sales" / "orders.parquet", lines)

    print("excel/")
    if not write_excel(
        ROOT / "excel" / "book.xlsx",
        {"Customers": people, "Orders": lines[:200]},
    ):
        print("  skipped — pip install openpyxl")
    else:
        write_excel(ROOT / "excel" / "budget.xlsx", {"Budget": people[:60]})
        # Two workbooks of the same shape in one subdirectory: the union, with the
        # file each row came from.
        write_excel(ROOT / "excel" / "monthly" / "january.xlsx", {"Orders": lines[:100]})
        write_excel(ROOT / "excel" / "monthly" / "february.xlsx", {"Orders": lines[100:200]})

    print(
        "\nRegister these in alkyon — a root file is a table, a subdirectory is one\n"
        "table over its files:\n"
        "  csv/           folder, type CSV\n"
        "  csv-european/  folder or file, CSV with delim ';' decimal ',' encoding latin-1 skip 2\n"
        "  json/          folder, type JSON\n"
        "  jsonl/         folder, type JSON lines\n"
        "  parquet/       folder, type Parquet\n"
        "  excel/         folder, type Excel\n"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
