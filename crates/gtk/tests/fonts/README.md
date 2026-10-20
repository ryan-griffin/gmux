# Shaping fixture

`GmuxLigatureTest.ttf` is a synthetic font used by `renderer-check`. It has
two-cell and three-cell ligatures in `liga`, and a contextual substitution in
`calt`. Its outlines and generator use the repository's MIT license.

The fixture makes the checks independent of installed fonts. Rebuild it with
Python and `fonttools==4.60.1` by running `python3 generate.py` in this directory.
