//! End-to-end tests: run the binary on hand-built and fixture PDFs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_pdf-redactor");

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pdf-redactor-tests-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn dump(path: &Path, extra: &[&str]) -> String {
    let mut args = vec!["-i", path.to_str().unwrap(), "--dump-text"];
    args.extend_from_slice(extra);
    let out = run(&args);
    assert!(out.status.success(), "dump failed: {}", String::from_utf8_lossy(&out.stderr));
    stdout(&out)
}

/// Runs a replacement and returns (process output, output file bytes if written).
fn replace(input: &Path, name: &str, extra: &[&str]) -> (Output, PathBuf) {
    let output = tmp(name);
    let _ = fs::remove_file(&output);
    let mut args = vec!["-i", input.to_str().unwrap(), "-o", output.to_str().unwrap()];
    args.extend_from_slice(extra);
    (run(&args), output)
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Builds a one-page PDF with a classic cross-reference table. `content` is
/// stored uncompressed so tests can look at the rewritten operators directly.
fn build_pdf(content: &str, fonts: &str, extra_objects: &[String], annots: &str) -> Vec<u8> {
    let mut objects: Vec<String> = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] /Contents 4 0 R \
             /Resources << /Font << {fonts} >> >> {annots} >>"
        ),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
    ];
    objects.extend(extra_objects.iter().cloned());
    let mut out = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for o in offsets {
        out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objects.len() + 1).as_bytes(),
    );
    out
}

const HELVETICA: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>";

fn simple(content: &str) -> PathBuf {
    let path = tmp(&format!("in-{:x}.pdf", content.len() * 31 + content.bytes().map(|b| b as usize).sum::<usize>()));
    fs::write(&path, build_pdf(content, "/F1 5 0 R", &[HELVETICA.to_string()], "")).unwrap();
    path
}

#[test]
fn replaces_simple_string_and_keeps_everything_else() {
    let input = simple("0.5 g\nBT /F1 12 Tf 20 150 Td (Hello Acme Corp. today) Tj ET\n10 10 50 50 re f");
    let (out, path) = replace(&input, "simple.pdf", &["-r", "Acme Corp.", "Initech"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("1 in text"));
    let bytes = fs::read(&path).unwrap();
    assert!(contains(&bytes, b"0.5 g\nBT /F1 12 Tf 20 150 Td (Hello Initech today) Tj ET\n10 10 50 50 re f"));
    assert!(!contains(&bytes, b"Acme"));
    // An uncompressed stream stays uncompressed, and no font was added.
    assert!(!contains(&bytes, b"FlateDecode"));
    assert!(!contains(&bytes, b"RdxF"));
    assert!(dump(&path, &[]).contains("Hello Initech today"));
}

#[test]
fn no_match_gives_identical_file() {
    let input = simple("BT /F1 12 Tf 20 150 Td (Nothing to see) Tj ET");
    let (out, path) = replace(&input, "nomatch.pdf", &["-r", "absent", "x"]);
    assert!(out.status.success());
    assert_eq!(fs::read(&input).unwrap(), fs::read(&path).unwrap());
}

#[test]
fn matches_across_kerned_array_and_keeps_unchanged_glyphs() {
    let input = simple("BT /F1 12 Tf 20 150 Td [(Rep)-15(ort f)10(or A)-20(cme C)5(orp)-300(now)] TJ ET");
    assert!(dump(&input, &[]).contains("Report for Acme Corp now"));
    let (out, path) = replace(&input, "kerned.pdf", &["-r", "Acme Corp", "Acme Inc"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let bytes = fs::read(&path).unwrap();
    // Only the differing tail is rewritten; earlier kerning is untouched.
    assert!(contains(&bytes, b"[(Rep)-15(ort f)10(or A)-20(cme "), "{}", String::from_utf8_lossy(&bytes));
    assert!(contains(&bytes, b"-300(now)] TJ"));
    assert!(dump(&path, &[]).contains("Report for Acme Inc now"));
}

#[test]
fn matches_across_operators_and_lines() {
    let input = simple("BT /F1 12 Tf 14 TL 20 150 Td (Send it to Acme) Tj T* (Corp. by Friday) Tj ET");
    assert!(dump(&input, &[]).contains("Send it to Acme\nCorp. by Friday"));
    let (out, path) = replace(&input, "lines.pdf", &["-r", "Acme Corp.", "Initech GmbH"]);
    assert!(out.status.success());
    let text = dump(&path, &[]);
    assert!(text.contains("Initech") && text.contains("GmbH") && !text.contains("Acme"), "{text}");
    assert!(text.contains("by Friday"));
}

#[test]
fn positioned_words_get_inferred_spaces() {
    let input = simple("BT /F1 12 Tf 20 150 Td (Acme) Tj 35 0 Td (Corp) Tj 200 0 Td (Far) Tj ET");
    assert!(dump(&input, &[]).contains("Acme Corp Far"));
    let (out, path) = replace(&input, "words.pdf", &["-r", "Acme Corp", "Initech"]);
    assert!(out.status.success());
    assert!(dump(&path, &[]).contains("Initech"));
}

#[test]
fn regex_rules_with_capture_groups_and_script_file() {
    let input = simple("BT /F1 12 Tf 20 150 Td (Invoice 2023-0042 due 2023-0107) Tj ET");
    let script = tmp("script.json");
    fs::write(
        &script,
        r#"{"rules": [
            {"search": "(\\d{4})-(\\d{4})", "replace": "$2/$1", "regex": true},
            {"search": "invoice", "replace": "Receipt", "ignore_case": true}
        ]}"#,
    )
    .unwrap();
    let (out, path) = replace(&input, "regex.pdf", &["-c", script.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(dump(&path, &[]).contains("Receipt 0042/2023 due 0107/2023"));
}

#[test]
fn hyperlinks_are_rewritten() {
    let annots = "/Annots [6 0 R]";
    let link = "<< /Type /Annot /Subtype /Link /Rect [0 0 10 10] /A << /S /URI /URI (https://acme.com/a?b=1) >> >>";
    let input = tmp("link-in.pdf");
    fs::write(
        &input,
        build_pdf("BT /F1 12 Tf 20 150 Td (see acme.com) Tj ET", "/F1 5 0 R", &[HELVETICA.into(), link.into()], annots),
    )
    .unwrap();
    let script = tmp("links.json");
    fs::write(&script, r#"[{"search": "acme.com", "replace": "initech.io", "targets": ["links"]}]"#).unwrap();
    let (out, path) = replace(&input, "link.pdf", &["-c", script.to_str().unwrap()]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("0 in text, 1 in links"));
    let bytes = fs::read(&path).unwrap();
    assert!(contains(&bytes, b"(https://initech.io/a?b=1)"));
    assert!(contains(&bytes, b"(see acme.com) Tj"), "text must be untouched when only links are targeted");
}

#[test]
fn hyperlink_stored_as_separate_string_object() {
    // macOS Quartz writes `/URI 7 0 R` with the URL in its own object.
    let link = "<< /Type /Annot /Subtype /Link /Rect [0 0 10 10] /A << /Type /Action /S /URI /URI 7 0 R >> >>";
    let uri = "(https://maps.example/search/519+Main+St)";
    let input = tmp("indirect-uri-in.pdf");
    fs::write(
        &input,
        build_pdf(
            "BT /F1 12 Tf 20 150 Td (Map) Tj ET",
            "/F1 5 0 R",
            &[HELVETICA.into(), link.into(), uri.into()],
            "/Annots [6 0 R]",
        ),
    )
    .unwrap();
    // --dump-text lists link targets after the page's text.
    assert!(dump(&input, &[]).contains("===== page 1 links =====\nhttps://maps.example/search/519+Main+St"));
    let (out, path) = replace(&input, "indirect-uri.pdf", &["-r", "519", "97"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("0 in text, 1 in links"), "{}", stdout(&out));
    let bytes = fs::read(&path).unwrap();
    assert!(contains(&bytes, b"(https://maps.example/search/97+Main+St)") && contains(&bytes, b"/URI 7 0 R"));
}

#[test]
fn missing_glyph_uses_fallback_or_is_reported() {
    // A Type 3 font that defines only the glyphs a, b, c.
    let font = "<< /Type /Font /Subtype /Type3 /FontBBox [0 0 1000 1000] /FontMatrix [0.001 0 0 0.001 0 0] \
                /CharProcs << /a 6 0 R /b 6 0 R /c 6 0 R >> \
                /Encoding << /Type /Encoding /Differences [97 /a /b /c] >> \
                /FirstChar 97 /LastChar 99 /Widths [600 600 600] >>";
    let glyph = "<< /Length 31 >>\nstream\n600 0 0 0 600 700 d1 0 0 600 700 re f\nendstream";
    let input = tmp("type3-in.pdf");
    fs::write(&input, build_pdf("BT /F1 12 Tf 20 150 Td (abcabc) Tj ET", "/F1 5 0 R", &[font.into(), glyph.into()], ""))
        .unwrap();

    // Glyphs the font has: stays in the original font.
    let (out, path) = replace(&input, "type3-a.pdf", &["-r", "abc", "cab"]);
    assert!(out.status.success());
    let bytes = fs::read(&path).unwrap();
    assert!(contains(&bytes, b"(cabcab) Tj") && !contains(&bytes, b"RdxF"));

    // Glyphs it lacks: a standard font is added to the page resources.
    let (out, path) = replace(&input, "type3-b.pdf", &["-r", "abc", "xyz"]);
    assert!(out.status.success());
    assert!(stdout(&out).contains("using a fallback font"));
    let bytes = fs::read(&path).unwrap();
    assert!(contains(&bytes, b"/BaseFont/Helvetica") && contains(&bytes, b"(xyz)Tj /F1 12 Tf"));

    // Fallback disabled: the match is reported and --strict refuses to write.
    let (out, path) = replace(&input, "type3-c.pdf", &["-r", "abc", "xyz", "--fallback-font", "none", "--strict"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no glyph for 'x'"));
    assert!(!path.exists());
}

#[test]
fn composite_font_with_tounicode_and_hex_strings() {
    let font = "<< /Type /Font /Subtype /Type0 /BaseFont /ABCDEF+Demo /Encoding /Identity-H \
                /DescendantFonts [<< /Type /Font /Subtype /CIDFontType2 /BaseFont /ABCDEF+Demo \
                /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /DW 500 \
                /FontDescriptor << /Type /FontDescriptor /FontName /ABCDEF+Demo /Flags 4 /FontFile2 7 0 R >> >>] \
                /ToUnicode 6 0 R >>";
    let cmap = "1 begincodespacerange <0000> <FFFF> endcodespacerange\n\
                6 beginbfchar <0001> <0048> <0002> <0069> <0003> <0020> <0004> <0042> <0005> <006F> <0006> <0062> endbfchar";
    let tounicode = format!("<< /Length {} >>\nstream\n{cmap}\nendstream", cmap.len());
    let fontfile = "<< /Length 0 >>\nstream\n\nendstream";
    let input = tmp("type0-in.pdf");
    // "Hi Bob"
    let content = "BT /F1 12 Tf 20 150 Td <000100020003000400050006> Tj ET";
    fs::write(&input, build_pdf(content, "/F1 5 0 R", &[font.into(), tounicode, fontfile.into()], "")).unwrap();
    assert!(dump(&input, &[]).contains("Hi Bob"));

    // "Bob" -> "Hob": H exists in the subset, so the original font is used, in hex.
    let (out, path) = replace(&input, "type0.pdf", &["-r", "Bob", "Hob"]);
    assert!(out.status.success());
    let bytes = fs::read(&path).unwrap();
    assert!(contains(&bytes, b"<000100020003000100050006> Tj"), "{}", String::from_utf8_lossy(&bytes));
    assert!(dump(&path, &[]).contains("Hi Hob"));
}

#[test]
fn fixtures_roundtrip_in_every_container_format() {
    for name in ["plain", "objstm", "linearized", "rc4", "aes128", "aes256"] {
        let input = fixture(&format!("{name}.pdf"));
        let before = dump(&input, &[]);
        assert!(before.contains("Confidential report for Acme Corp."), "{name}: {before}");
        let (out, path) = replace(
            &input,
            &format!("fx-{name}.pdf"),
            &["-r", "Acme Corp.", "Initech AG.", "-r", "acme.com", "init.io"],
        );
        assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
        assert!(stdout(&out).contains("2 in text"), "{name}: {}", stdout(&out));
        assert!(stdout(&out).contains("1 in links"), "{name}: {}", stdout(&out));
        let after = dump(&path, &[]);
        assert!(after.contains("Confidential report for Initech AG."), "{name}: {after}");
        assert!(after.contains("Page two also mentions Initech AG. here."), "{name}: {after}");
        assert!(after.contains("jane@init.io") && !after.contains("Acme") && !after.contains("acme"), "{name}");
        let (in_len, out_len) = (fs::metadata(&input).unwrap().len(), fs::metadata(&path).unwrap().len());
        assert!(out_len <= in_len, "{name}: grew from {in_len} to {out_len}");
        let bytes = fs::read(&path).unwrap();
        let encrypted = ["rc4", "aes128", "aes256"].contains(&name);
        assert_eq!(contains(&bytes, b"/Encrypt"), encrypted, "{name}: encryption must be preserved");
        assert_eq!(contains(&bytes, b"/ObjStm"), contains(&fs::read(&input).unwrap(), b"/ObjStm"), "{name}");
    }
}

#[test]
fn password_protected_file() {
    let input = fixture("aes256_password.pdf");
    let (out, path) = replace(&input, "pw-none.pdf", &["-r", "Acme", "X"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a password"));
    assert!(!path.exists());

    let (out, path) = replace(&input, "pw.pdf", &["-r", "Acme Corp.", "Initech AG.", "-p", "hunter2"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(dump(&path, &["-p", "hunter2"]).contains("Initech AG."));
    // Still locked with the same password.
    assert!(!run(&["-i", path.to_str().unwrap(), "--dump-text"]).status.success());
}

#[test]
fn damaged_cross_reference_is_recovered() {
    let mut bytes = build_pdf("BT /F1 12 Tf 20 150 Td (Hello Acme) Tj ET", "/F1 5 0 R", &[HELVETICA.to_string()], "");
    let at = bytes.windows(9).rposition(|w| w == b"startxref").unwrap();
    bytes.truncate(at);
    bytes.extend_from_slice(b"startxref\n99999\n%%EOF\n");
    let input = tmp("damaged-in.pdf");
    fs::write(&input, bytes).unwrap();
    let (out, path) = replace(&input, "damaged.pdf", &["-r", "Acme", "Init"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("reconstructed"));
    assert!(dump(&path, &[]).contains("Hello Init"));
}

#[test]
fn actual_text_follows_the_glyphs() {
    let input = simple("BT /F1 12 Tf 20 150 Td /Span <</ActualText (Acme Corp)>> BDC (Acme Corp) Tj EMC ET");
    let (out, path) = replace(&input, "actual.pdf", &["-r", "Acme Corp", "Initech"]);
    assert!(out.status.success());
    let bytes = fs::read(&path).unwrap();
    assert!(contains(&bytes, b"<</ActualText (Initech)>> BDC (Initech) Tj"), "{}", String::from_utf8_lossy(&bytes));
}

#[test]
fn page_ranges_and_dry_run() {
    let input = fixture("plain.pdf");
    let script = tmp("pages.json");
    fs::write(&script, r#"{"rules": [{"search": "Acme Corp.", "replace": "Initech", "pages": "2"}]}"#).unwrap();
    let (out, path) = replace(&input, "pages.pdf", &["-c", script.to_str().unwrap()]);
    assert!(out.status.success());
    let text = dump(&path, &[]);
    assert!(text.contains("report for Acme Corp.") && text.contains("mentions Initech here"), "{text}");

    let out = run(&["-i", input.to_str().unwrap(), "--dry-run", "-r", "Acme", "X"]);
    assert!(out.status.success() && stdout(&out).contains("2 in text") && stdout(&out).contains("nothing written"));
}
