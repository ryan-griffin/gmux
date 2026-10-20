"""Rebuild the synthetic shaping fixture with fonttools 4.60.1."""
from pathlib import Path

from fontTools.feaLib.builder import addOpenTypeFeaturesFromString
from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen


def outline(rectangles):
    pen = TTGlyphPen(None)
    for left, bottom, right, top in rectangles:
        pen.moveTo((left, bottom))
        pen.lineTo((left, top))
        pen.lineTo((right, top))
        pen.lineTo((right, bottom))
        pen.closePath()
    return pen.glyph()


shapes = {
    ".notdef": [(100, 0, 500, 700)],
    "space": [],
    "M": [(80, 0, 180, 700), (420, 0, 520, 700), (180, 500, 420, 600)],
    "X": [(100, 0, 200, 700), (400, 0, 500, 700), (200, 300, 400, 400)],
    "f": [(200, 0, 300, 700), (100, 450, 450, 550)],
    "i": [(250, 0, 350, 450), (250, 600, 350, 700)],
    "hyphen": [(100, 300, 500, 400)],
    "greater": [(150, 100, 250, 250), (250, 250, 450, 450), (150, 450, 250, 600)],
    "less": [(350, 100, 450, 250), (150, 250, 350, 450), (350, 450, 450, 600)],
    "equal": [(100, 200, 500, 300), (100, 450, 500, 550)],
    "exclam": [(250, 200, 350, 700), (250, 0, 350, 100)],
    "exclam.alt": [(100, 0, 200, 700), (200, 200, 600, 300)],
    "arrow": [(100, 300, 1100, 400), (800, 100, 900, 600)],
    "ffi": [(100, 0, 1700, 100), (100, 100, 200, 700), (800, 100, 900, 700)],
}
font = FontBuilder(1000, isTTF=True)
font.setupGlyphOrder(list(shapes))
font.setupCharacterMap({
    ord(" "): "space",
    ord("M"): "M",
    ord("X"): "X",
    ord("f"): "f",
    ord("i"): "i",
    ord("-"): "hyphen",
    ord(">"): "greater",
    ord("<"): "less",
    ord("="): "equal",
    ord("!"): "exclam",
})
font.setupGlyf({name: outline(rectangles) for name, rectangles in shapes.items()})
advances = {"arrow": 1200, "ffi": 1800}
font.setupHorizontalMetrics({name: (advances.get(name, 600), 0) for name in shapes})
font.setupHorizontalHeader(ascent=800, descent=-200)
font.setupNameTable({
    "familyName": "Gmux Ligature Test",
    "styleName": "Regular",
    "uniqueFontIdentifier": "GmuxLigatureTest-Regular",
    "fullName": "Gmux Ligature Test Regular",
    "psName": "GmuxLigatureTest-Regular",
})
font.setupOS2(sTypoAscender=800, sTypoDescender=-200, usWinAscent=800, usWinDescent=200)
font.setupPost(isFixedPitch=1)
addOpenTypeFeaturesFromString(font.font, """
languagesystem DFLT dflt;
languagesystem latn dflt;
feature liga {
    sub hyphen greater by arrow;
    sub f f i by ffi;
} liga;
feature calt {
    sub exclam' equal by exclam.alt;
} calt;
""")
font.font.recalcTimestamp = False
font.font["head"].created = font.font["head"].modified = 2082844800
font.save(Path(__file__).with_name("GmuxLigatureTest.ttf"))
