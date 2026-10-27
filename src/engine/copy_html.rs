//! Adapt the pinned formatter's HTML, without converting terminal cells/styles.
//!
//! Its styled path keeps whitespace that plain formatting trims, wraps each
//! page in a block, and can cross style/hyperlink tag boundaries. Use inline
//! wrappers, omit links, resolve its palette CSS, and reconcile whitespace with
//! the authoritative plain copy. Reject any other text difference atomically.
use super::ffi::GmuxCopyColors;
use gmux_client::clipboard::MAX_HTML;

pub(super) fn normalize(raw: &str, plain: &str, colors: &GmuxCopyColors) -> Option<String> {
    let mut output = String::new();
    // Palette references shrink, while div/span tags grow slightly. Allow
    // room for those inline tags and the outer wrapper without another scan.
    output
        .try_reserve_exact(
            raw.len()
                .saturating_add(raw.len() / 16)
                .saturating_add(128)
                .min(MAX_HTML),
        )
        .ok()?;
    append(
        &mut output,
        "<div style=\"font-family:monospace;white-space:pre;color:",
    )?;
    append_color(&mut output, colors.foreground)?;
    append(&mut output, ";background-color:")?;
    append_color(&mut output, colors.background)?;
    append(&mut output, "\">")?;
    let mut rest = raw;
    let mut expected = plain;
    let mut depth = 0usize;
    // After a failed batch match, inspect that run only once. Repeated scans
    // would make a long run with trimmed trailing whitespace quadratic.
    let mut literal_remaining = 0usize;
    while !rest.is_empty() {
        if let Some(tag) = rest.strip_prefix('<') {
            let end = tag.find('>')?;
            let tag = &tag[..end];
            if let Some(style) = tag
                .strip_prefix("div style=\"")
                .and_then(|s| s.strip_suffix('"'))
            {
                append(&mut output, "<span style=\"")?;
                let mut style = style;
                while let Some((before, after)) = style.split_once("var(--vt-palette-") {
                    append(&mut output, before)?;
                    let (index, after) = after.split_once(')')?;
                    let color = colors.palette.get(index.parse::<usize>().ok()?)?;
                    append_color(&mut output, *color)?;
                    style = after;
                }
                append(&mut output, style)?;
                append(&mut output, "\">")?;
                depth += 1;
            } else if tag == "/div" {
                depth = depth.checked_sub(1)?;
                append(&mut output, "</span>")?;
            } else if tag != "/a" && !tag.starts_with("a href=\"") {
                return None;
            }
            rest = &rest[end + 2..];
        } else {
            // Ghostty emits non-ASCII text as entities. Copy matching ASCII
            // runs together; trimming differences still use the reconciliation
            // below. NUL and CR do not round-trip through an HTML parser.
            if literal_remaining == 0 && !rest.starts_with('&') {
                let end = if matches!(rest.as_bytes().get(1), Some(b'<' | b'&')) {
                    1
                } else {
                    rest.find(['<', '&']).unwrap_or(rest.len())
                };
                let text = &rest[..end];
                let matching = text.len().min(expected.len());
                literal_remaining = end;
                if matching > 0
                    && text.is_ascii()
                    && !text.contains(['\0', '\r'])
                    && let Some(suffix) = expected.strip_prefix(&text[..matching])
                {
                    append(&mut output, &text[..matching])?;
                    expected = suffix;
                    rest = &rest[matching..];
                    literal_remaining -= matching;
                    continue;
                }
            }
            let (character, token) = if rest.starts_with('&') {
                let end = rest.find(';')?;
                let entity = &rest[1..end];
                let character = match entity {
                    "lt" => '<',
                    "gt" => '>',
                    "amp" => '&',
                    "quot" => '"',
                    "#39" => '\'',
                    _ => char::from_u32(entity.strip_prefix('#')?.parse().ok()?)?,
                };
                (character, &rest[..end + 1])
            } else {
                let character = rest.chars().next()?;
                (character, &rest[..character.len_utf8()])
            };
            // HTML remaps C1 numeric references and NUL, and normalizes CR.
            if matches!(character, '\0' | '\r') || ('\u{80}'..='\u{9f}').contains(&character) {
                return None;
            }
            if character == ' ' || character == '\n' {
                if expected.starts_with(character) {
                    append(&mut output, token)?;
                    expected = &expected[1..];
                }
            } else {
                let whitespace = expected.len() - expected.trim_start_matches([' ', '\n']).len();
                append(&mut output, &expected[..whitespace])?;
                expected = expected[whitespace..].strip_prefix(character)?;
                append(&mut output, token)?;
            }
            rest = &rest[token.len()..];
            literal_remaining = literal_remaining.saturating_sub(token.len());
        }
    }
    if depth != 0 || !expected.bytes().all(|b| b == b' ' || b == b'\n') {
        return None;
    }
    append(&mut output, expected)?;
    append(&mut output, "</div>")?;
    Some(output)
}

fn append(output: &mut String, text: &str) -> Option<()> {
    let required = output.len().checked_add(text.len())?;
    if required > MAX_HTML {
        return None;
    }
    if required > output.capacity() {
        let capacity = output
            .capacity()
            .saturating_mul(2)
            .max(required)
            .min(MAX_HTML);
        output.try_reserve_exact(capacity - output.len()).ok()?;
    }
    output.push_str(text);
    Some(())
}

fn append_color(output: &mut String, rgb: [u8; 3]) -> Option<()> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut color = *b"#000000";
    for (index, component) in rgb.into_iter().enumerate() {
        color[1 + index * 2] = HEX[usize::from(component >> 4)];
        color[2 + index * 2] = HEX[usize::from(component & 15)];
    }
    append(output, std::str::from_utf8(&color).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_runs_preserve_whitespace_reconciliation() {
        let colors = GmuxCopyColors {
            foreground: [255; 3],
            background: [0; 3],
            palette: [[0; 3]; 256],
        };
        let text = "x".repeat(100_000);
        for (actual, expected) in [
            (format!("{text}   "), text.clone()),
            (format!("{text}   after"), format!("{text}\nafter")),
            (format!("before   {text}"), format!("before\n{text}")),
        ] {
            let raw = format!("<div style=\"\">{actual}</div>");
            let output = normalize(&raw, &expected, &colors).unwrap();
            assert!(output.ends_with(&format!("<span style=\"\">{expected}</span></div>")));
        }
    }

    #[test]
    fn incompatible_formatter_output_and_normalized_expansion_are_discarded() {
        let colors = GmuxCopyColors {
            foreground: [255; 3],
            background: [0; 3],
            palette: [[0; 3]; 256],
        };
        for (raw, plain) in [
            ("<div style=\"\">different</div>", "expected"),
            ("<script>x</script>", "x"),
            ("<div style=\"\">&#128;</div>", "\u{80}"),
            ("<div style=\"\">before\rafter</div>", "before\rafter"),
            ("<div style=\"\">x", "x"),
        ] {
            assert!(normalize(raw, plain, &colors).is_none());
        }
        // Inline wrappers are larger than Ghostty's shortest block wrappers.
        // Bound the final result independently of the raw formatter budget.
        let wrapper = "<div style=\"\">x</div>";
        let count = MAX_HTML / wrapper.len();
        assert!(normalize(&wrapper.repeat(count), &"x".repeat(count), &colors).is_none());
    }
}
