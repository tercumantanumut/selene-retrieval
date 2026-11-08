"""Generate benign and hostile fixtures for the M0-C extract sandbox spike.

Nothing here is downloaded; every hostile input is synthesised locally.
Usage: python gen_fixtures.py [outdir] [--extra]
"""
import os
import sys
import zlib
import zipfile

OUT = sys.argv[1] if len(sys.argv) > 1 and not sys.argv[1].startswith("--") else os.path.join(os.path.dirname(__file__), "fixtures")
os.makedirs(OUT, exist_ok=True)


def w(name, data):
    with open(os.path.join(OUT, name), "wb") as f:
        f.write(data)


def build_pdf(objs):
    """objs: list of bytes bodies for objects 1..n. Object 1 must be the catalog."""
    out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for i, body in enumerate(objs, start=1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % i + body + b"\nendobj\n"
    xref = len(out)
    out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(objs) + 1)
    for o in offsets:
        out += b"%010d 00000 n \n" % o
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(objs) + 1, xref)
    return bytes(out)


def stream(content, filt=None, raw_len=None):
    hdr = b"<< /Length %d" % (raw_len if raw_len is not None else len(content))
    if filt:
        hdr += b" /Filter /" + filt
    return hdr + b" >>\nstream\n" + content + b"\nendstream"


def text_page_content(lines):
    c = b"BT /F1 12 Tf 72 720 Td 14 TL\n"
    for ln in lines:
        c += b"(" + ln.encode("latin-1") + b") '\n"
    return c + b"ET"


def sample_pdf(pages=3):
    # 1 catalog, 2 pages, 3 font, then (page, content) pairs
    kids = []
    objs = [None, None, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"]
    for p in range(pages):
        page_no = len(objs) + 1
        kids.append(page_no)
        objs.append(b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents %d 0 R >>" % (page_no + 1))
        lines = [f"Selene retrieval spike page {p + 1}.", "The quick brown fox jumps over the lazy dog.", f"Keyword: pdfium-ort-coexist-{p + 1}"]
        objs.append(stream(zlib.compress(text_page_content(lines)), b"FlateDecode"))
    objs[0] = b"<< /Type /Catalog /Pages 2 0 R >>"
    objs[1] = b"<< /Type /Pages /Kids [" + b" ".join(b"%d 0 R" % k for k in kids) + b"] /Count %d >>" % len(kids)
    return build_pdf(objs)



def extras():
    """Huge single-page PDFs and pathological text (run alone with --extra)."""
    import random
    # Huge single page: 200x200 inch MediaBox scaled by UserUnit 75000, 300k positioned glyph runs.
    ops = bytearray(b"BT /F1 1 Tf ")
    for i in range(300_000):
        ops += b"1 0 0 1 %d %d Tm (W%d) Tj " % (i % 14400, (i * 7) % 14400, i % 10)
    ops += b"ET"
    w("pdf-huge-page.pdf", build_pdf([
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 14400 14400] /UserUnit 75000 /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        stream(zlib.compress(bytes(ops), 9), b"FlateDecode"),
    ]))
    # Single page with 3M vector path ops (content-stream parse cost, no text).
    paths = b"0 0 m 1 1 l S " * 3_000_000
    w("pdf-huge-paths.pdf", build_pdf([
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>",
        stream(zlib.compress(paths, 9), b"FlateDecode"),
    ]))
    # Pathological text.
    w("long-line.txt", b"a" * (48 << 20))  # 48 MiB, no newline -> output truncation
    w("zalgo.txt", ("a" + "\u0301" * 1_000_000).encode())  # 1M combining marks on one base char
    rnd = random.Random(7)
    bad = [b"\xc0\x80", b"\xed\xa0\x80", b"\xff", b"\xf8\x88\x80\x80\x80", b"\xe2\x82", b"ok "]
    w("invalid-utf8.txt", b"".join(rnd.choice(bad) for _ in range(500_000)))
    w("utf16-odd.txt", b"\xff\xfe" + "UTF-16 text \U0001F600 ".encode("utf-16-le") * 10_000 + b"\x00")
    w("bidi-zw.txt", ("\u202e\u200b\u2066evil\u2069\ufeff" * 200_000).encode())
    w("oversize.txt", b"x" * (80 << 20))  # above the 64 MiB payload cap


if "--extra" in sys.argv:
    extras()
    sys.exit(0)

# ---------- benign ----------
pdf = sample_pdf(3)
w("sample.pdf", pdf)
w("big-200p.pdf", sample_pdf(200))
w("sample.html", b"<!doctype html><html><head><title>T</title><style>p{}</style><script>var x=1;</script></head><body><h1>Heading</h1><p>Hello <b>HTML</b> world &amp; friends.</p><ul><li>one</li><li>two</li></ul><table><tr><td>a</td><td>b</td></tr></table></body></html>")
w("sample.md", b"# Title\n\nSome *markdown* with `code` and a [link](http://x).\n\n- item one\n- item two\n\n```rust\nfn main() {}\n```\n")


def docx(document_xml, name, extra=None, compress=zipfile.ZIP_DEFLATED):
    path = os.path.join(OUT, name)
    with zipfile.ZipFile(path, "w", compress) as z:
        z.writestr("[Content_Types].xml", '<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>')
        z.writestr("_rels/.rels", '<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>')
        if callable(document_xml):
            with z.open("word/document.xml", "w", force_zip64=True) as f:
                document_xml(f)
        else:
            z.writestr("word/document.xml", document_xml)
        for k, v in (extra or {}).items():
            z.writestr(k, v)


NS = 'xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"'
docx(f'<?xml version="1.0" encoding="UTF-8"?><w:document {NS}><w:body><w:p><w:r><w:t>Hello DOCX world.</w:t></w:r></w:p><w:p><w:r><w:t xml:space="preserve">Second </w:t></w:r><w:r><w:t>paragraph &amp; more.</w:t></w:r></w:p></w:body></w:document>', "sample.docx")

# ---------- hostile ----------
w("zero.pdf", b"")
w("zero.txt", b"")
w("truncated.pdf", pdf[: len(pdf) // 2])
w("binary.txt", os.urandom(1 << 20))
w("binary-as.pdf", os.urandom(1 << 20))
w("binary-as.docx", os.urandom(1 << 16))
w("binary-as.html", os.urandom(1 << 20))


# Zip bomb disguised as .docx: word/document.xml inflates to ~3 GiB of one <w:t>.
def bomb(f):
    f.write(f'<?xml version="1.0"?><w:document {NS}><w:body><w:p><w:r><w:t>'.encode())
    chunk = b"A" * (1 << 24)
    for _ in range(192):  # 192 * 16 MiB = 3 GiB
        f.write(chunk)
    f.write(b"</w:t></w:r></w:p></w:body></w:document>")


docx(bomb, "zipbomb.docx")

# Deeply nested XML inside a docx (1,000,000 levels), plus billion-laughs entities.
depth = 1_000_000
docx(f'<?xml version="1.0"?><w:document {NS}><w:body>' + "<w:p>" * depth + "<w:r><w:t>deep</w:t></w:r>" + "</w:p>" * depth + "</w:body></w:document>", "nested-xml.docx")
lol = '<?xml version="1.0"?><!DOCTYPE lolz [<!ENTITY lol "lol">' + "".join(
    f'<!ENTITY lol{i} "' + (f"&lol{i - 1};" if i > 1 else "&lol;") * 10 + '">' for i in range(1, 10)) + f']><w:document {NS}><w:body><w:p><w:r><w:t>&lol9;</w:t></w:r></w:p></w:body></w:document>'
docx(lol, "billion-laughs.docx")

# Deeply nested HTML (stack-depth attack on DOM walkers) and markdown.
w("nested.html", b"<html><body>" + b"<div>" * 200_000 + b"deep" + b"</div>" * 200_000 + b"</body></html>")
w("nested.md", b">" * 200_000 + b" deep\n" + b"[" * 100_000 + b"x" + b"](y)" * 1000 + b"\n")

# PDF whose single page content stream is a deflate bomb (~2 GiB of text operators).
def deflate_bomb(n_mib):
    co = zlib.compressobj(9)
    parts = [co.compress(b"BT /F1 12 Tf 72 720 Td ")]
    blk = (b"(A) Tj " * 150_000)[: (1 << 20) - 1] + b" "
    for _ in range(n_mib):
        parts.append(co.compress(blk))
    parts.append(co.compress(b" ET"))
    parts.append(co.flush())
    return b"".join(parts)


bomb_stream = deflate_bomb(2048)
w("pdf-flate-bomb.pdf", build_pdf([
    b"<< /Type /Catalog /Pages 2 0 R >>",
    b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>",
    b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    stream(bomb_stream, b"FlateDecode"),
]))

# PDF with a huge object stream (/Type /ObjStm) holding 2,000,000 small objects.
n = 2_000_000
hdr = b" ".join(b"%d %d" % (10 + i, i * 4) for i in range(n))
body = b"".join(b"[1] " for _ in range(n))
objstm = zlib.compress(hdr + b" " + body, 6)
w("pdf-huge-objstm.pdf", build_pdf([
    b"<< /Type /Catalog /Pages 2 0 R >>",
    b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
    b"<< /Type /ObjStm /N %d /First %d /Length %d /Filter /FlateDecode >>\nstream\n" % (n, len(hdr) + 1, len(objstm)) + objstm + b"\nendstream",
]))

# PDF with a cyclic page tree (Pages -> Kids -> itself) and a self-referencing /Prev xref.
cyc = build_pdf([
    b"<< /Type /Catalog /Pages 2 0 R >>",
    b"<< /Type /Pages /Kids [2 0 R 3 0 R] /Count 1000000 >>",
    b"<< /Type /Pages /Parent 2 0 R /Kids [2 0 R 3 0 R] /Count 1000000 >>",
])
cyc = cyc.replace(b"/Root 1 0 R >>", b"/Root 1 0 R /Prev %d >>" % cyc.rfind(b"xref"))
w("pdf-cyclic.pdf", cyc)

# PDF with a 1,000,000-deep nested array (recursive-descent parser attack).
w("pdf-deep-array.pdf", build_pdf([
    b"<< /Type /Catalog /Pages 2 0 R /X " + b"[" * 1_000_000 + b"]" * 1_000_000 + b" >>",
    b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
    b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] >>",
]))

extras()

for f in sorted(os.listdir(OUT)):
    print(f"{os.path.getsize(os.path.join(OUT, f)):>12}  {f}")
