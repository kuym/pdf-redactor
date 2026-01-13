# pdf-redactor

Scripted search and replace for the **rendered text** and **hyperlink URLs** of
a PDF.

The tool edits only text-showing operators (`Tj`, `TJ`, `'`, `"`) and URI
actions.  Everything else is copied from the input byte-for-byte: object
numbers, object streams, cross-reference style, compression, images, fonts and
encryption all stay as they were.

```
pdf-redactor -i in.pdf -o out.pdf -c script.json
pdf-redactor -i in.pdf -o out.pdf -r "Acme Corp." "Initech LLC" -r acme.com initech.io
pdf-redactor -i in.pdf --dump-text            # text and link URIs exactly as the matcher sees them
pdf-redactor -i in.pdf --dry-run -c script.json
```

## Script format

```json
{
  "rules": [
    { "search": "Acme Corp.", "replace": "Initech LLC" },
    { "search": "(\\d{4})-(\\d{2})-(\\d{2})", "replace": "$3.$2.$1", "regex": true },
    { "search": "confidential", "replace": "public", "ignore_case": true, "whole_word": true },
    { "search": "acme.com", "replace": "initech.io", "targets": ["links"] },
    { "search": "Draft", "replace": "Final", "pages": "1-3,7" }
  ]
}
```

| field         | default             | meaning                                                     |
|---------------|---------------------|-------------------------------------------------------------|
| `search`      | required            | literal text, or a regular expression when `regex` is true  |
| `replace`     | required            | replacement; with `regex`, `$1` / `${name}` refer to groups |
| `regex`       | `false`             | Rust `regex` syntax                                         |
| `ignore_case` | `false`             |                                                             |
| `whole_word`  | `false`             | match only at word boundaries                               |
| `targets`     | `["text", "links"]` | where the rule applies                                      |
| `pages`       | all                 | page ranges for text, e.g. `"1-3,7,10-"`                    |

Rules run in order; each rule sees the result of the ones before it. A bare
list of rules and the short form `{"replacements": {"old": "new"}}` are also
accepted. `-r SEARCH REPLACE` adds literal rules after the script's.

In a literal `search`, any whitespace matches any run of whitespace, including a
line break. PDFs rarely store spaces and line ends as characters, so the matcher
infers them from where glyphs sit on the page.

## How text is matched and rewritten

1. Every content stream that can draw text is interpreted: pages, form
   XObjects, tiling patterns and annotation appearance streams.
2. Character codes are mapped to Unicode through each font's `ToUnicode` CMap
   or its encoding, and glyph positions are tracked. Words split across
   operators, kerning arrays or lines are joined into one searchable text.
3. A match is mapped back to the glyphs that drew it. Glyphs the replacement
   leaves unchanged are not touched, so `Acme Corp` → `Acme Inc` rewrites only
   `Corp`. The new text goes where the first changed glyph was.
4. Only the affected operators are spliced into the stream. All other bytes of
   the stream are preserved, and the stream is re-deflated with
   [Zopfli](https://en.wikipedia.org/wiki/Zopfli).

Replacement text is encoded with glyphs the font is known to contain. Embedded
subset fonts hold only the glyphs the document already uses; if a needed glyph
is missing, the replacement is set in the matching standard font (Helvetica,
Times or Courier, in the right weight and slant), which every PDF reader
provides and which costs one small object. `--fallback-font none` skips such
replacements instead and reports them; `--strict` then makes that an error.

`--fit auto` (the default) condenses a rewritten run horizontally only when a
longer replacement would collide with text that follows it on the same line.
`--fit strict` never lets a run grow; `--fit none` always uses natural widths.
Text is not reflowed: a much longer replacement at the end of a line can extend
past the original right edge.

## What the output preserves

- Untouched objects are byte-identical, at the same object numbers and in the
  same order. Pages without matches render pixel-identically.
- Object streams and cross-reference streams are kept if the input used them;
  a classic `xref` table stays a classic table.
- Uncompressed streams stay uncompressed; compressed ones are written as
  [Flate](https://www.rfc-editor.org/info/rfc1951/).
- Encrypted files ([RC4](https://en.wikipedia.org/wiki/RC4),
  [AES-128, AES-256](https://en.wikipedia.org/wiki/Advanced_Encryption_Standard))
  stay encrypted with the same keys and permissions. Use `--password` if opening
  the file needs one.
- If nothing matches, the output is the input bit for bit.
- File size does not grow beyond what longer replacement text requires. If
  adding a fallback font would make the file larger, the size guard losslessly
  re-deflates other Flate streams to win the bytes back (`--allow-growth`
  turns this off).

Three things necessarily change when a file is rewritten: superseded revisions
from incremental updates are dropped (so replaced text cannot be recovered from
them), a [linearized ("fast web view")](https://qpdf.readthedocs.io/en/stable/linearization.html)
file becomes a normal one because its hint tables would be wrong, and a damaged
cross-reference table is rebuilt.

## Scope and limits

- Only rendered text and URI actions are changed. Document metadata (Info, XMP),
  bookmarks, named destinations, form field values, annotation comments and
  the structure tree are deliberately left alone. Inline `/ActualText` on
  marked content is updated together with the glyphs it describes.
- Text drawn as vector outlines or contained in images is not text and cannot
  be found.
- Fonts with no usable [Unicode](https://en.wikipedia.org/wiki/Unicode) mapping
  (no `ToUnicode`, non-standard encoding) are not searchable; `--dump-text`
  shows such glyphs as `�`.
- Right-to-left and vertical scripts are matched in stream order only.
- [Digital signatures](https://en.wikipedia.org/wiki/Digital_signature) are
  invalidated by any change to a signed document.

## Building and testing

```
cargo build --release      # binary: target/release/pdf-redactor
cargo test
```

# License

Apache 2.0 License

Copyright (C) 2026, Kuy Mainwaring (https://github.com/kuym)
