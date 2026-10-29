//! Command-line order of clap options (base64/basenc and the checksum tools).

use clap::ArgMatches;
use clap::parser::ValueSource;

/// Index of the last occurrence of `id` on the command line, if any.
pub(crate) fn last_index(m: &ArgMatches, id: &str) -> Option<usize> {
    if m.value_source(id) != Some(ValueSource::CommandLine) {
        return None;
    }
    m.indices_of(id).and_then(|i| i.max())
}
