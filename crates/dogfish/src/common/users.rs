//! User and group name lookups.

use std::ffi::CStr;

pub fn get_username_by_uid(uid: u32) -> Option<String> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static UID_CACHE: RefCell<HashMap<u32, Option<String>>> = RefCell::new(HashMap::new());
    }
    UID_CACHE.with(|cache| {
        if let Some(cached) = cache.borrow().get(&uid) {
            return cached.clone();
        }
        let resolved = unsafe {
            let pw = libc::getpwuid(uid);
            if pw.is_null() || (*pw).pw_name.is_null() {
                None
            } else {
                Some(CStr::from_ptr((*pw).pw_name).to_string_lossy().into_owned())
            }
        };
        cache.borrow_mut().insert(uid, resolved.clone());
        resolved
    })
}

pub fn get_groupname_by_gid(gid: u32) -> Option<String> {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static GID_CACHE: RefCell<HashMap<u32, Option<String>>> = RefCell::new(HashMap::new());
    }
    GID_CACHE.with(|cache| {
        if let Some(cached) = cache.borrow().get(&gid) {
            return cached.clone();
        }
        let resolved = unsafe {
            let gr = libc::getgrgid(gid);
            if gr.is_null() || (*gr).gr_name.is_null() {
                None
            } else {
                Some(CStr::from_ptr((*gr).gr_name).to_string_lossy().into_owned())
            }
        };
        cache.borrow_mut().insert(gid, resolved.clone());
        resolved
    })
}
