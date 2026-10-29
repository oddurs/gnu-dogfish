//! The locale's character set, decided as glibc does: whether characters
//! are UTF-8 sequences (`MB_CUR_MAX > 1`) or single bytes (the C/POSIX
//! locale, or any locale that does not exist).

use std::sync::OnceLock;

/// True when `LC_ALL` / `LC_CTYPE` / `LANG` select an installed UTF-8
/// locale: `setlocale (LC_CTYPE, "")`, then `nl_langinfo (CODESET)`.
pub(crate) fn utf8_locale() -> bool {
    static UTF8: OnceLock<bool> = OnceLock::new();
    *UTF8.get_or_init(|| {
        // Safety: only LC_CTYPE is touched, and it is put back to "C" so
        // nothing else sees a changed locale; the codeset is read before.
        unsafe {
            if libc::setlocale(libc::LC_CTYPE, c"".as_ptr()).is_null() {
                return false;
            }
            let codeset = libc::nl_langinfo(libc::CODESET);
            let utf8 = !codeset.is_null() && {
                let name = std::ffi::CStr::from_ptr(codeset).to_bytes();
                name.eq_ignore_ascii_case(b"UTF-8") || name.eq_ignore_ascii_case(b"utf8")
            };
            libc::setlocale(libc::LC_CTYPE, c"C".as_ptr());
            utf8
        }
    })
}
