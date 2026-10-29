//! getopt-style option parsing for the file tools.

use crate::common::quote::quote_locale;

/// Whether an option takes an argument, as in `getopt_long`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HasArg {
    No,
    Required,
    Optional,
}

/// One option: any of the letters in `shorts` and/or the `long` name.
pub(crate) struct ToolOpt<T> {
    shorts: &'static str,
    pub(crate) long: &'static str,
    pub(crate) arg: HasArg,
    pub(crate) id: T,
}

pub(crate) const fn opt<T>(
    shorts: &'static str,
    long: &'static str,
    arg: HasArg,
    id: T,
) -> ToolOpt<T> {
    ToolOpt {
        shorts,
        long,
        arg,
        id,
    }
}

/// Parsed command line: options in the order given (so later flags can override
/// earlier ones, as in GNU) and the operands.
pub(crate) struct ToolArgs<T> {
    pub(crate) opts: Vec<(T, Option<String>)>,
    pub(crate) operands: Vec<String>,
}

/// Prints a usage error the way every GNU utility does; returns exit status 1.
pub(crate) fn usage_error(tool: &str, msg: &str) -> i32 {
    eprintln!("{tool}: {msg}\nTry '{tool} --help' for more information.");
    1
}

enum LongMatch<'a, T> {
    Spec(&'a ToolOpt<T>),
    Help,
    Version,
}

/// Resolves a (possibly abbreviated) long option name. `Err` lists the
/// candidates: none means unrecognized, several means ambiguous.
fn find_long<'a, T: Copy + PartialEq>(
    name: &str,
    specs: &'a [ToolOpt<T>],
) -> std::result::Result<(&'static str, LongMatch<'a, T>), Vec<&'static str>> {
    let mut candidates: Vec<(&'static str, LongMatch<'a, T>)> = specs
        .iter()
        .filter(|s| !s.long.is_empty())
        .map(|s| (s.long, LongMatch::Spec(s)))
        .collect();
    candidates.push(("help", LongMatch::Help));
    candidates.push(("version", LongMatch::Version));
    if let Some(i) = candidates.iter().position(|(long, _)| *long == name) {
        return Ok(candidates.swap_remove(i));
    }
    let mut matches: Vec<_> = candidates
        .into_iter()
        .filter(|(long, _)| long.starts_with(name))
        .collect();
    let same_option = |a: &LongMatch<T>, b: &LongMatch<T>| match (a, b) {
        (LongMatch::Spec(x), LongMatch::Spec(y)) => x.id == y.id && x.arg == y.arg,
        _ => false,
    };
    if matches.len() == 1
        || (!matches.is_empty() && matches.iter().all(|m| same_option(&m.1, &matches[0].1)))
    {
        return Ok(matches.swap_remove(0));
    }
    Err(matches.into_iter().map(|(long, _)| long).collect())
}

/// Parses `args` (including `argv[0]`) like glibc's `getopt_long`: options and
/// operands may be interleaved, short options bundle, long options may be
/// abbreviated, and `--` ends option processing. `--help` and `--version` are
/// handled here. `Err` carries the exit status after help, version or a usage
/// error has been printed.
pub(crate) fn parse_tool_args<T: Copy + PartialEq>(
    tool: &str,
    args: &[String],
    specs: &[ToolOpt<T>],
    help: &str,
) -> std::result::Result<ToolArgs<T>, i32> {
    getopt_impl(tool, args, specs, help, false)
}

/// Like [`parse_tool_args`], but options end at the first operand (getopt's `+`
/// mode, used by `basename`).
pub(crate) fn parse_tool_args_posix<T: Copy + PartialEq>(
    tool: &str,
    args: &[String],
    specs: &[ToolOpt<T>],
    help: &str,
) -> std::result::Result<ToolArgs<T>, i32> {
    getopt_impl(tool, args, specs, help, true)
}

fn getopt_impl<T: Copy + PartialEq>(
    tool: &str,
    args: &[String],
    specs: &[ToolOpt<T>],
    help: &str,
    stop_at_operand: bool,
) -> std::result::Result<ToolArgs<T>, i32> {
    let mut opts = Vec::new();
    let mut operands = Vec::new();
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if arg == "--" {
            operands.extend(rest.cloned());
            break;
        }
        if let Some(body) = arg.strip_prefix("--") {
            let (name, value) = match body.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (body, None),
            };
            let (full, found) = match find_long(name, specs) {
                Ok(m) => m,
                Err(c) if c.is_empty() => {
                    return Err(usage_error(tool, &format!("unrecognized option '{arg}'")));
                }
                Err(c) => {
                    let list: Vec<String> = c.iter().map(|l| format!("'--{l}'")).collect();
                    return Err(usage_error(
                        tool,
                        &format!(
                            "option '--{name}' is ambiguous; possibilities: {}",
                            list.join(" ")
                        ),
                    ));
                }
            };
            let has_arg = match &found {
                LongMatch::Spec(s) => s.arg,
                _ => HasArg::No,
            };
            let value = match (has_arg, value) {
                (HasArg::No, Some(_)) => {
                    return Err(usage_error(
                        tool,
                        &format!("option '--{full}' doesn't allow an argument"),
                    ));
                }
                (HasArg::Required, None) => match rest.next() {
                    Some(v) => Some(v.clone()),
                    None => {
                        return Err(usage_error(
                            tool,
                            &format!("option '--{full}' requires an argument"),
                        ));
                    }
                },
                (_, v) => v,
            };
            match found {
                LongMatch::Spec(s) => opts.push((s.id, value)),
                LongMatch::Help => {
                    print!("{help}");
                    return Err(0);
                }
                LongMatch::Version => {
                    println!("{tool} (GNU/Dogfish) {}", env!("CARGO_PKG_VERSION"));
                    return Err(0);
                }
            }
        } else if arg.len() > 1 && arg.starts_with('-') {
            let body = &arg[1..];
            for (idx, c) in body.char_indices() {
                let Some(spec) = specs.iter().find(|s| s.shorts.contains(c)) else {
                    return Err(usage_error(tool, &format!("invalid option -- '{c}'")));
                };
                let tail = &body[idx + c.len_utf8()..];
                match spec.arg {
                    HasArg::No => opts.push((spec.id, None)),
                    HasArg::Optional => {
                        opts.push((spec.id, (!tail.is_empty()).then(|| tail.to_string())));
                        break;
                    }
                    HasArg::Required => {
                        let value = if !tail.is_empty() {
                            tail.to_string()
                        } else if let Some(v) = rest.next() {
                            v.clone()
                        } else {
                            return Err(usage_error(
                                tool,
                                &format!("option requires an argument -- '{c}'"),
                            ));
                        };
                        opts.push((spec.id, Some(value)));
                        break;
                    }
                }
            }
        } else {
            operands.push(arg.clone());
            if stop_at_operand {
                operands.extend(rest.cloned());
                break;
            }
        }
    }
    Ok(ToolArgs { opts, operands })
}

/// Matches `arg` (or an unambiguous prefix of it) against `table`, like GNU
/// `argmatch`; prints GNU's diagnostic and returns `Err(1)` otherwise.
pub(crate) fn argmatch<V: Copy + PartialEq>(
    tool: &str,
    context: &str,
    arg: &str,
    table: &[(&str, V)],
) -> std::result::Result<V, i32> {
    if let Some((_, v)) = table.iter().find(|(name, _)| *name == arg) {
        return Ok(*v);
    }
    let matches: Vec<V> = table
        .iter()
        .filter(|(name, _)| name.starts_with(arg) && !arg.is_empty())
        .map(|(_, v)| *v)
        .collect();
    if let Some(first) = matches.first()
        && matches.iter().all(|v| v == first)
    {
        return Ok(*first);
    }
    let problem = if matches.is_empty() {
        "invalid"
    } else {
        "ambiguous"
    };
    let mut msg = format!(
        "{tool}: {problem} argument {} for {}\nValid arguments are:",
        quote_locale(arg),
        quote_locale(context)
    );
    let mut prev: Option<V> = None;
    for (name, v) in table {
        if prev == Some(*v) {
            msg.push_str(&format!(", '{name}'"));
        } else {
            msg.push_str(&format!("\n  - '{name}'"));
        }
        prev = Some(*v);
    }
    eprintln!("{msg}\nTry '{tool} --help' for more information.");
    Err(1)
}
