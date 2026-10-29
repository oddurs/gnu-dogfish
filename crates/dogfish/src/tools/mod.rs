//! One module per utility. Each exports the `run_*` entry point `main` dispatches to.

pub(crate) mod basename;
pub(crate) mod chmod;
pub(crate) mod chown;
pub(crate) mod cp;
pub(crate) mod df;
pub(crate) mod dirname;
pub(crate) mod du;
pub(crate) mod ln;
pub(crate) mod ls;
pub(crate) mod mkdir;
pub(crate) mod mktemp;
pub(crate) mod mv;
pub(crate) mod realpath;
pub(crate) mod rm;
pub(crate) mod stat;
pub(crate) mod touch;
