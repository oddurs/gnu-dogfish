//! `sort --debug`: GNU's warnings about the keys (`key_warnings`) and the
//! underlining of each output line's keys (`debug_line`).

use unicode_width::UnicodeWidthChar;

use super::compare::{
    NONE, SortKey, SortOpts, Sorter, blank, month_of, traverse_raw_number, unit_order,
};
use super::ldbl;
use crate::common::locale::utf8_locale;
use crate::common::quote::quote_locale;

fn warn(msg: &str) {
    eprintln!("sort: {msg}");
}

/// The messages GNU prints first: whether the locale could be set, and how
/// text is ordered. Dogfish always compares bytes, as GNU does in the C
/// locale.
pub(super) fn locale_messages() {
    // SAFETY: NUL-terminated literals; the locale is put back to "C" at
    // once, before anything else could depend on it.
    let ok = unsafe {
        let ok = !libc::setlocale(libc::LC_ALL, c"".as_ptr()).is_null()
            && !libc::setlocale(libc::LC_COLLATE, c"".as_ptr()).is_null();
        libc::setlocale(libc::LC_ALL, c"C".as_ptr());
        ok
    };
    if !ok {
        warn("failed to set locale");
    }
    warn("text ordering performed using simple byte comparison");
}

/// GNU `key_warnings`. `gkey` holds the global options; `gkey_only` says
/// they make up the only key.
pub(super) fn key_warnings(
    keys: &[SortKey],
    gkey: &SortOpts,
    gkey_only: bool,
    tab: Option<u8>,
    stable_or_unique: bool,
) {
    // The C locale's decimal point; it has no thousands separator.
    const DECIMAL_POINT: u8 = b'.';
    let mut ugkey = *gkey;
    let mut basic_numeric_field = false;
    let mut general_numeric_field = false;
    let mut basic_numeric_field_span = false;
    let mut general_numeric_field_span = false;

    for (i, key) in keys.iter().enumerate() {
        let keynum = i + 1;
        let o = &key.opts;
        if o.key_numeric() {
            if o.general {
                general_numeric_field = true;
            } else {
                basic_numeric_field = true;
            }
        }

        if key.traditional {
            // +A.x -B.y is -k A+1.x+1,B.y (y = 0) or -k A+1.x+1,B+1.y.
            let sword = key.sword.wrapping_add(usize::from(key.sword == NONE));
            let mut obsolete = format!("+{sword}");
            let mut modern = format!("-k {}", sword.wrapping_add(1));
            if key.eword != NONE {
                obsolete.push_str(&format!(" -{}", key.eword.wrapping_add(1)));
                modern.push_str(&format!(
                    ",{}",
                    key.eword
                        .wrapping_add(1)
                        .wrapping_add(usize::from(key.echar == NONE))
                ));
            }
            warn(&format!(
                "obsolescent key {} used; consider {} instead",
                quote_locale(&obsolete),
                quote_locale(&modern)
            ));
        }

        // Field specs that will never match.
        let zero_width = key.sword != NONE && key.eword < key.sword;
        if zero_width {
            warn(&format!("key {keynum} has zero width and will be ignored"));
        }

        // Significant leading blanks.
        let implicit_skip = o.key_numeric() || o.month;
        let line_offset = key.eword == 0 && key.echar != 0; // -k1.x,1.y
        if !zero_width
            && !gkey_only
            && tab.is_none()
            && !line_offset
            && ((!o.skipsblanks && !implicit_skip)
                || (!o.skipsblanks && key.schar != 0)
                || (!o.skipeblanks && key.echar != 0))
        {
            warn(&format!(
                "leading blanks are significant in key {keynum}; consider also specifying 'b'"
            ));
        }

        // Numeric keys spanning fields: a field delimiter could be read as
        // part of the number.
        if !gkey_only && o.key_numeric() {
            let sword = key.sword.wrapping_add(1).max(1);
            let eword = key.eword.wrapping_add(1);
            if eword == 0 || sword < eword {
                warn(&format!(
                    "key {keynum} is numeric and spans multiple fields"
                ));
                if o.general {
                    general_numeric_field_span = true;
                } else {
                    basic_numeric_field_span = true;
                }
            }
        }

        // Global options not copied to or given in any key.
        if ugkey.ignore == o.ignore {
            ugkey.ignore = Default::default();
        }
        ugkey.translate &= !o.translate;
        ugkey.skipsblanks &= !o.skipsblanks;
        ugkey.skipeblanks &= !o.skipeblanks;
        ugkey.month &= !o.month;
        ugkey.numeric &= !o.numeric;
        ugkey.general &= !o.general;
        ugkey.human &= !o.human;
        ugkey.random &= !o.random;
        ugkey.version &= !o.version;
        ugkey.reverse &= !o.reverse;
    }

    // Field delimiters that do not constrain numbers.
    let mut number_locale_warned = false;
    if basic_numeric_field_span || general_numeric_field_span {
        let quoted = |c: u8| quote_locale(std::ffi::OsStr::new(&(c as char).to_string()));
        if tab == Some(DECIMAL_POINT) {
            warn(&format!(
                "field separator {} is treated as a decimal point in numbers",
                quoted(DECIMAL_POINT)
            ));
            number_locale_warned = true;
        } else if tab == Some(b'-') {
            warn(&format!(
                "field separator {} is treated as a minus sign in numbers",
                quoted(b'-')
            ));
        } else if general_numeric_field_span && tab == Some(b'+') {
            warn(&format!(
                "field separator {} is treated as a plus sign in numbers",
                quoted(b'+')
            ));
        }
    }

    // The decimal point in use: robust scripts set the locale for numbers.
    if (basic_numeric_field || general_numeric_field) && !number_locale_warned {
        warn(&format!(
            "{}numbers use {} as a decimal point in this locale",
            if tab == Some(DECIMAL_POINT) {
                ""
            } else {
                "note "
            },
            quote_locale(".")
        ));
    }

    // Ignored global options.
    if !ugkey.is_default() || (ugkey.reverse && stable_or_unique && !keys.is_empty()) {
        let reverse = ugkey.reverse;
        if !stable_or_unique {
            ugkey.reverse = false;
        }
        let opts = ugkey.letters();
        if opts.len() == 1 {
            warn(&format!("option '-{opts}' is ignored"));
        } else {
            warn(&format!("options '-{opts}' are ignored"));
        }
        ugkey.reverse = reverse;
    }
    if ugkey.reverse && !stable_or_unique && !keys.is_empty() {
        warn("option '-r' only applies to last-resort comparison");
    }
}

/// GNU `debug_width`: the columns `text` takes, each tab counting as one
/// (it is shown as `>`).
fn debug_width(text: &[u8]) -> usize {
    let tabs = text.iter().filter(|&&c| c == b'\t').count();
    tabs + if utf8_locale() {
        let mut width = 0;
        for chunk in text.utf8_chunks() {
            for c in chunk.valid().chars() {
                width += c.width().unwrap_or(0);
            }
            width += chunk.invalid().len();
        }
        width
    } else {
        // mbsnwidth in a single-byte locale: control characters take no
        // column, every other byte one.
        text.iter().filter(|&&c| !(c < 0x20 || c == 0x7f)).count()
    }
}

/// GNU `mark_key`.
fn mark_key(out: &mut Vec<u8>, offset: usize, width: usize) {
    out.resize(out.len() + offset, b' ');
    if width == 0 {
        out.extend_from_slice(b"^ no match for key\n");
    } else {
        out.resize(out.len() + width, b'_');
        out.push(b'\n');
    }
}

/// GNU `debug_key`: underlines `key` in `line`, or the whole line.
fn debug_key(sorter: &Sorter, out: &mut Vec<u8>, line: &[u8], key: Option<&SortKey>) {
    let mut beg = 0;
    let mut lim = line.len();
    if let Some(key) = key {
        let o = &key.opts;
        if key.sword != NONE {
            beg = sorter.begfield(line, key);
        }
        if key.eword != NONE {
            lim = sorter.limfield(line, key);
        }
        if (o.skipsblanks && key.sword == NONE) || o.month || o.key_numeric() {
            // GNU ends the string at the limit; one that lies before the
            // start leaves the line's own end.
            let end = if lim < beg { line.len() } else { lim };
            while beg < end && blank(line[beg]) {
                beg += 1;
            }
            lim = if lim < beg {
                lim
            } else if o.month {
                match month_of(&line[beg..lim]) {
                    (0, _) => beg,
                    (_, len) => beg + len,
                }
            } else if o.general {
                beg + ldbl::strtold_key(&line[beg..lim]).1
            } else if o.numeric || o.human {
                let text = &line[..lim];
                let from = beg + usize::from(beg < lim && line[beg] == b'-');
                match traverse_raw_number(text, from) {
                    (p, _, true) => p + usize::from(o.human && unit_order(text.get(p)) != 0),
                    _ => beg,
                }
            } else {
                lim
            };
        }
    }
    let offset = debug_width(&line[..beg]);
    let width = if lim > beg {
        debug_width(&line[beg..lim])
    } else {
        0
    };
    mark_key(out, offset, width);
}

/// GNU `write_line` in debug mode: the line with tabs shown as `>` and a
/// newline for its terminator, then each key underlined, then (unless `-s`
/// or `-u`) the whole line for the last-resort comparison.
pub(super) fn debug_line(sorter: &Sorter, out: &mut Vec<u8>, line: &[u8]) {
    out.extend(line.iter().map(|&c| if c == b'\t' { b'>' } else { c }));
    out.push(b'\n');
    for key in &sorter.keys {
        debug_key(sorter, out, line, Some(key));
    }
    if sorter.keys.is_empty() || !sorter.no_last_resort {
        debug_key(sorter, out, line, None);
    }
}
