// TODO: Add support for LOCALE_NAME_USER_DEFAULT

#![cfg(windows)]

use std::borrow::Cow;
use std::collections::HashSet;
use std::ffi::CStr;
use std::mem;
use std::ptr;
use std::sync::{Arc, Mutex};

use num_format_windows::{
    LOCALE_NAME_MAX_LENGTH, LOCALE_NAME_SYSTEM_DEFAULT, LOCALE_SDECIMAL, LOCALE_SGROUPING,
    LOCALE_SNAME, LOCALE_SNAN, LOCALE_SNEGATIVESIGN, LOCALE_SNEGINFINITY, LOCALE_SPOSINFINITY,
    LOCALE_SPOSITIVESIGN, LOCALE_STHOUSAND,
};
use widestring::{U16CStr, U16CString};
use winapi::ctypes::c_int;
use winapi::shared::minwindef::{BOOL, DWORD, LPARAM};
use winapi::um::errhandlingapi::GetLastError;
use winapi::um::winnls;
use winapi::um::winnt::WCHAR;

use crate::error::Error;
use crate::grouping::Grouping;
use crate::strings::{DecString, InfString, MinString, NanString, PosString, SepString};
use crate::system_locale::SystemLocale;

lazy_static! {
    static ref SYSTEM_DEFAULT: Result<&'static str, Error> = {
        CStr::from_bytes_with_nul(LOCALE_NAME_SYSTEM_DEFAULT).map_err(|_| {
            Error::windows(
                "LOCALE_NAME_SYSTEM_DEFAULT from windows.h unexpectedly contains interior nul byte.",
            )
        })?.to_str().map_err(|_| {
            Error::windows(
                "LOCALE_NAME_SYSTEM_DEFAULT from windows.h unexpectedly contains invalid UTF-8.",
            )
        })
    };
}

pub(crate) fn available_names() -> Result<HashSet<String>, Error> {
    // call safe wrapper for EnumSystemLocalesEx
    // see https://docs.microsoft.com/en-us/windows/desktop/api/winnls/nf-winnls-enumsystemlocalesex
    enum_system_locales_ex()
}

pub(crate) fn new(name: Option<String>) -> Result<SystemLocale, Error> {
    let name: Cow<str> = match name {
        Some(name) => name.into(),
        None => (*SYSTEM_DEFAULT)?.into(),
    };

    let max_len = LOCALE_NAME_MAX_LENGTH as usize - 1;
    if name.len() > max_len {
        return Err(Error::windows(format!(
            "locale names on windows may not exceed {} bytes (including a null byte).",
            max_len,
        )));
    }

    let dec = {
        let s = get_locale_info_ex(&name, Request::Decimal)?;
        DecString::new(&s).map_err(|_| Error::windows("TODO"))?
    };

    let grp = {
        let grp_string = get_locale_info_ex(&name, Request::Grouping)?;
        match grp_string.as_ref() {
            "3;0" | "3" => Grouping::Standard,
            "3;2;0" | "3;2" => Grouping::Indian,
            "" => Grouping::Posix,
            _ => {
                return Err(Error::windows(format!(
                "for the locale {:?}, windows returned a grouping value of {:?}, which num-format \
                does not currently support. if you need support this grouping value, please file \
                an issue at https://github.com/bcmyers/num-format.",
                &name, &grp_string)));
            }
        }
    };

    let inf = {
        let s = get_locale_info_ex(&name, Request::PositiveInfinity)?;
        InfString::new(&s).map_err(|_| {
            Error::windows(format!(
                "for the locale {:?}, windows returned an infinity sign of length {} bytes, \
                 which exceeds the maximum length for infinity signs that num-format currently \
                 supports. if you need support longer infinity signs, please file an \
                 issue at https://github.com/bcmyers/num-format.",
                &name,
                s.len()
            ))
        })?
    };

    let min = {
        let s = get_locale_info_ex(&name, Request::MinusSign)?;
        MinString::new(&s).map_err(|_| {
            Error::windows(format!(
                "for the locale {:?}, windows returned a minus sign of length {} bytes, \
                 which exceeds the maximum length for minus signs that num-format currently \
                 supports. if you need support longer minus signs, please file an issue \
                 at https://github.com/bcmyers/num-format.",
                &name,
                s.len()
            ))
        })?
    };

    let nan = {
        let s = get_locale_info_ex(&name, Request::Nan)?;
        NanString::new(&s).map_err(|_| {
            Error::windows(format!(
                "for the locale {:?}, windows returned a NaN value of length {} bytes, \
                 which exceeds the maximum length for NaN values that num-format currently \
                 supports. if you need support longer NaN values, please file an issue \
                 at https://github.com/bcmyers/num-format.",
                &name,
                s.len()
            ))
        })?
    };

    let sep = {
        let s = get_locale_info_ex(&name, Request::Separator)?;
        SepString::new(&s).map_err(|_| Error::windows("TODO"))?
    };

    let pos = {
        let s = get_locale_info_ex(&name, Request::PositiveSign)?;
        PosString::new(&s).map_err(|_| Error::windows("TODO"))?
    };

    // we already have the name unless unless it was LOCALE_NAME_SYSTEM_DEFAULT, a special
    // case that doesn't correspond to our concept of name. in this special case, we have
    // to ask windows for the user-friendly name. the unwrap is OK, because we would never
    // have reached this point if LOCALE_NAME_SYSTEM_DEFAULT were an error.
    let name = if &name == SYSTEM_DEFAULT.as_ref().unwrap() {
        get_locale_info_ex(&name, Request::Name)?
    } else {
        name.into()
    };

    let locale = SystemLocale {
        dec,
        grp,
        inf,
        min,
        name,
        nan,
        pos,
        sep,
    };

    Ok(locale)
}

/// Enum representing all the things we know how to ask Windows for via the GetLocaleInfoEx API.
#[derive(Copy, Clone, Debug)]
pub enum Request {
    Decimal,
    Grouping,
    MinusSign,
    Name,
    Nan,
    NegativeInfinity,
    PositiveInfinity,
    PositiveSign,
    Separator,
}

impl From<Request> for DWORD {
    fn from(request: Request) -> DWORD {
        use self::Request::*;
        match request {
            Decimal => LOCALE_SDECIMAL,
            Grouping => LOCALE_SGROUPING,
            MinusSign => LOCALE_SNEGATIVESIGN,
            Name => LOCALE_SNAME,
            Nan => LOCALE_SNAN,
            NegativeInfinity => LOCALE_SNEGINFINITY,
            PositiveInfinity => LOCALE_SPOSINFINITY,
            PositiveSign => LOCALE_SPOSITIVESIGN,
            Separator => LOCALE_STHOUSAND,
        }
    }
}

/// Safe wrapper for EnumSystemLocalesEx.
/// See https://docs.microsoft.com/en-us/windows/desktop/api/winnls/nf-winnls-enumsystemlocalesex.
fn enum_system_locales_ex() -> Result<HashSet<String>, Error> {
    // global variables needed because we need to populate a HashSet inside a C callback function.
    lazy_static! {
        static ref OUTER_MUTEX: Arc<Mutex<()>> = Arc::new(Mutex::new(()));
        static ref INNER_MUTEX: Arc<Mutex<HashSet<String>>> =
            Arc::new(Mutex::new(HashSet::default()));
    }

    // callback function.
    #[allow(non_snake_case)]
    unsafe extern "system" fn lpLocaleEnumProcEx(
        lpLocaleName: *mut WCHAR,
        _: DWORD,
        _: LPARAM,
    ) -> BOOL {
        // will be called continuously by windows until 0 is returned
        const CONTINUE: BOOL = 1;
        const STOP: BOOL = 0;

        if lpLocaleName.is_null() {
            return STOP;
        }

        let s = match U16CStr::from_ptr_str(lpLocaleName).to_string() {
            Ok(s) => s,
            Err(_) => return CONTINUE,
        };

        if &s == "" {
            return CONTINUE;
        }

        let mut inner_guard = INNER_MUTEX.lock().unwrap();
        let _ = inner_guard.insert(s);

        CONTINUE
    };

    let set = {
        let outer_guard = OUTER_MUTEX.lock().unwrap();
        {
            let mut inner_guard = INNER_MUTEX.lock().unwrap();
            inner_guard.clear();
        }
        let ret =
            unsafe { winnls::EnumSystemLocalesEx(Some(lpLocaleEnumProcEx), 0, 0, ptr::null_mut()) };
        if ret == 0 {
            let err = unsafe { GetLastError() };
            return Err(Error::windows(format!(
                "EnumSystemLocalesEx failed with error code: {}",
                err
            )));
        }
        let set = {
            let inner_guard = INNER_MUTEX.lock().unwrap();
            inner_guard.clone()
        };
        drop(outer_guard);
        set
    };

    Ok(set)
}

/// Safe wrapper for GetLocaleInfoEx.
/// See https://docs.microsoft.com/en-us/windows/desktop/api/winnls/nf-winnls-getlocaleinfoex.
fn get_locale_info_ex(locale_name: &str, request: Request) -> Result<String, Error> {
    const BUF_LEN: usize = 1024;

    // inner function that represents actual call to GetLocaleInfoEx (will be used twice below)
    #[allow(non_snake_case)]
    fn inner(
        lpLocaleName: *const WCHAR,
        LCType: DWORD,
        buf_ptr: *mut WCHAR,
        size: c_int,
    ) -> Result<c_int, Error> {
        let size = unsafe { winnls::GetLocaleInfoEx(lpLocaleName, LCType, buf_ptr, size) };
        if size == 0 {
            let err = unsafe { GetLastError() };
            return Err(Error::windows(format!(
                "GetLocaleInfoEx failed with error code: {}",
                err
            )));
        } else if size < 0 {
            return Err(Error::windows(format!(
                "GetLocaleInfoEx unexpectedly returned a negative value of {}",
                size
            )));
        }
        // cast is OK because we've already checked that size is positive
        if size as usize > BUF_LEN {
            return Err(Error::windows(format!(
                "GetLocaleInfoEx wants to write a string of {} WCHARs, which num-format does not \
                 currently support (current max is {}). if you would like num-format to support \
                 GetLocaleInfoEx writing longer strings, please please file an issue at \
                 https://github.com/bcmyers/num-format.",
                size, BUF_LEN,
            )));
        }
        Ok(size)
    }

    // turn locale_name into windows string
    let locale_name = U16CString::from_str(locale_name).map_err(|_| Error::windows("TODO"))?;

    #[allow(non_snake_case)]
    let lpLocaleName = locale_name.as_ptr();

    #[allow(non_snake_case)]
    let LCType = DWORD::from(request);

    // first call to GetLocaleInfoEx to get size of the data it will write.
    let size = inner(lpLocaleName, LCType, ptr::null_mut(), 0)?;

    // second call to GetLocaleInfoEx to write data into our buffer.
    let mut buf: [WCHAR; BUF_LEN] = unsafe { mem::uninitialized() };
    let written = inner(lpLocaleName, LCType, buf.as_mut_ptr(), size)?;
    if written != size {
        return Err(Error::windows(
            "unexpected return value from GetLocaleInfoEx.",
        ));
    }

    let s = U16CStr::from_slice_with_nul(&buf[..written as usize])
        .map_err(|_| {
            Error::windows("data written by GetLocaleInfoEx unexpectedly missing null byte.")
        })?
        .to_string()
        .map_err(|_| {
            Error::windows("data written by GetLocaleInfoEx unexpectedly contains invalid UTF-16.")
        })?;

    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_locale_windows_constructors() {
        let locale = new(None).unwrap();
        println!("DEFAULT LOCALE NAME: {}", locale.name());
        let _ = new(Some("en-US".to_string())).unwrap();
        let names = available_names().unwrap();
        for name in names {
            let _ = new(Some(name)).unwrap();
        }
    }

    #[test]
    fn test_system_locale_windows_available_names() {
        use std::sync::mpsc;
        use std::thread;

        let locales = available_names().unwrap();

        let (sender, receiver) = mpsc::channel();

        let mut handles = Vec::new();
        for _ in 0..20 {
            let sender = sender.clone();
            let handle = thread::spawn(move || {
                let locales = enum_system_locales_ex().unwrap();
                sender.send(locales).unwrap();
            });
            handles.push(handle);
        }

        let mut localess = Vec::new();
        for _ in handles {
            let locales = receiver.recv().unwrap();
            localess.push(locales);
        }

        for locales2 in localess {
            assert_eq!(locales, locales2)
        }
    }
}