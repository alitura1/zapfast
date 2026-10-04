#!/usr/bin/env python3
"""Reports how complete each translation catalog is against the template.

The catalogs are the source of truth for their own text; this only counts the
entries in `assets/i18n/*.po` that have no translation yet, so a release can see
what is missing without opening every file. `update-translations.sh` is still
what generates the source references; nothing here rewrites a catalog.

Usage: translation-coverage.py [--min PERCENT]
Exits 1 when any catalog is below --min (default 0, i.e. report only).
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
I18N = ROOT / "assets" / "i18n"

ENTRY = re.compile(r"(?m)^(?:msgctxt .*\n)?(?:#.*\n)*msgid ")


def entries(text: str):
    """Yields ((msgctxt, msgid), has_translation, fuzzy) for each real entry.

    The key is the context together with the id, because the catalogs carry entries that differ
    only by context (`Calls` as a page and as a noun, and the label-colour names). Keying on the
    id alone collapsed those pairs, so an untranslated contextual entry hid behind its plain twin
    and the template looked smaller than it is.
    """
    for block in re.split(r"\n\n+", text):
        if not block.strip() or block.startswith("#~"):
            continue
        ctxts = re.findall(r'(?m)^msgctxt (".*"(?:\n".*")*)', block)
        ctxt = ctxts[0] if ctxts else ""
        ids = re.findall(r'(?m)^msgid (".*"(?:\n".*")*)', block)
        if not ids or ids[0] == '""':
            continue
        plural = "msgstr[0]" in block
        if plural:
            translated = all(
                value.strip() != '""'
                for value in re.findall(r'(?m)^msgstr\[\d+\] (".*")', block)
            )
        else:
            values = re.findall(r'(?m)^msgstr (".*"(?:\n".*")*)', block)
            translated = bool(values) and values[0].strip() != '""'
        yield (ctxt, ids[0]), translated, bool(re.search(r"(?m)^#, .*fuzzy", block))


def main() -> int:
    minimum = 0.0
    if "--min" in sys.argv:
        minimum = float(sys.argv[sys.argv.index("--min") + 1])

    template = (I18N / "zapfast.pot").read_text(encoding="utf-8")
    defined = {key for key, _, _ in entries(template)}
    print(f"template: {len(defined)} messages")

    failed = False
    for path in sorted(I18N.glob("*.po")):
        text = path.read_text(encoding="utf-8")
        seen = {}
        fuzzy = 0
        obsolete = len(re.findall(r"(?m)^#~ msgid ", text))
        for key, translated, is_fuzzy in entries(text):
            # A fuzzy entry is present but not approved by a translator, so it must not count
            # toward coverage; it is reported separately as `fuzzy`.
            seen[key] = translated and not is_fuzzy
            fuzzy += is_fuzzy
        translated = sum(1 for state in seen.values() if state)
        untranslated = sum(1 for state in seen.values() if not state)
        missing = len(defined - seen.keys())
        coverage = 100.0 * translated / len(defined) if defined else 100.0
        print(
            f"{path.name:12} {coverage:5.1f}%  "
            f"translated={translated:3} untranslated={untranslated:3} "
            f"fuzzy={fuzzy:2} missing={missing:2} obsolete={obsolete:2}"
        )
        if coverage < minimum:
            failed = True
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
