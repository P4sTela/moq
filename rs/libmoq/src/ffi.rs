use std::ffi::{c_char, c_void, CStr};

use url::Url;

use crate::{Error, Id};

pub struct Callback {
	user_data: *mut c_void,
	on_status: Option<extern "C" fn(user_data: *mut c_void, code: i32)>,
}

impl Callback {
	pub unsafe fn new(
		user_data: *mut c_void,
		on_status: Option<extern "C" fn(user_data: *mut c_void, code: i32)>,
	) -> Self {
		Self { user_data, on_status }
	}

	pub fn call<C: ReturnCode>(&mut self, ret: C) {
		if let Some(on_status) = &self.on_status {
			on_status(self.user_data, ret.code());
		}
	}
}

unsafe impl Send for Callback {}

/// A callback that delivers a raw byte frame.
///
/// NOTE: `call` is invoked from the background tokio runtime thread, NOT the
/// caller's thread. The C# side must marshal to its main thread as needed.
/// The `data` pointer and `size` are only valid for the duration of the call.
pub struct DataCallback {
	user_data: *mut c_void,
	on_data: Option<extern "C" fn(user_data: *mut c_void, data: *const u8, size: usize)>,
}

impl DataCallback {
	pub unsafe fn new(
		user_data: *mut c_void,
		on_data: Option<extern "C" fn(user_data: *mut c_void, data: *const u8, size: usize)>,
	) -> Self {
		Self { user_data, on_data }
	}

	pub fn call(&self, data: &[u8]) {
		if let Some(on_data) = &self.on_data {
			on_data(self.user_data, data.as_ptr(), data.len());
		}
	}
}

// Safety: the C caller is responsible for ensuring user_data outlives the
// subscription and is safe to access from the runtime thread.
unsafe impl Send for DataCallback {}

/// A callback that delivers broadcast (un)announcements.
///
/// NOTE: `call` is invoked from the background tokio runtime thread, NOT the
/// caller's thread. `path` is a freshly-allocated null-terminated C string only
/// valid for the duration of the call.
pub struct AnnounceCallback {
	user_data: *mut c_void,
	on_announce: Option<extern "C" fn(user_data: *mut c_void, path: *const c_char, active: i32)>,
}

impl AnnounceCallback {
	pub unsafe fn new(
		user_data: *mut c_void,
		on_announce: Option<extern "C" fn(user_data: *mut c_void, path: *const c_char, active: i32)>,
	) -> Self {
		Self {
			user_data,
			on_announce,
		}
	}

	pub fn call(&self, path: &str, active: bool) {
		if let Some(on_announce) = &self.on_announce {
			// Allocate a null-terminated C string valid for the duration of the call.
			let cstr = std::ffi::CString::new(path).unwrap_or_default();
			on_announce(self.user_data, cstr.as_ptr(), active as i32);
		}
	}
}

unsafe impl Send for AnnounceCallback {}

pub fn return_code<C: ReturnCode, F: FnOnce() -> C>(f: F) -> i32 {
	match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
		Ok(ret) => ret.code(),
		Err(_) => Error::Panic.code(),
	}
}

pub trait ReturnCode {
	fn code(&self) -> i32;
}

impl ReturnCode for () {
	fn code(&self) -> i32 {
		0
	}
}

impl ReturnCode for i32 {
	fn code(&self) -> i32 {
		*self
	}
}

impl ReturnCode for Result<i32, Error> {
	fn code(&self) -> i32 {
		match self {
			Ok(code) if *code < 0 => Error::InvalidCode.code(),
			Ok(code) => *code,
			Err(e) => e.code(),
		}
	}
}

impl ReturnCode for Result<usize, Error> {
	fn code(&self) -> i32 {
		match self {
			Ok(code) => i32::try_from(*code).unwrap_or_else(|_| Error::InvalidCode.code()),
			Err(e) => e.code(),
		}
	}
}

impl ReturnCode for Result<Id, Error> {
	fn code(&self) -> i32 {
		match self {
			Ok(id) => i32::try_from(*id).unwrap_or_else(|_| Error::InvalidCode.code()),
			Err(e) => e.code(),
		}
	}
}

impl ReturnCode for Result<(), Error> {
	fn code(&self) -> i32 {
		match self {
			Ok(()) => 0,
			Err(e) => e.code(),
		}
	}
}

impl ReturnCode for usize {
	fn code(&self) -> i32 {
		i32::try_from(*self).unwrap_or_else(|_| Error::InvalidCode.code())
	}
}

impl ReturnCode for Id {
	fn code(&self) -> i32 {
		i32::try_from(*self).unwrap_or_else(|_| Error::InvalidCode.code())
	}
}

pub fn parse_id(id: i32) -> Result<Id, Error> {
	Id::try_from(id)
}

pub fn parse_url(url: *const c_char) -> Result<Url, Error> {
	if url.is_null() {
		return Err(Error::InvalidPointer);
	}

	let url = unsafe { CStr::from_ptr(url) };
	let url = url.to_str()?;
	Ok(Url::parse(url)?)
}

/// # Safety
///
/// The caller must ensure that cstr is valid for 'a.
pub unsafe fn parse_str<'a>(cstr: *const c_char) -> Result<&'a str, Error> {
	if cstr.is_null() {
		return Ok("");
	}

	let string = unsafe { CStr::from_ptr(cstr) };
	Ok(string.to_str()?)
}

/// # Safety
///
/// The caller must ensure that data is valid for 'a.
pub unsafe fn parse_slice<'a>(data: *const u8, size: usize) -> Result<&'a [u8], Error> {
	if data.is_null() {
		if size == 0 {
			return Ok(&[]);
		}

		return Err(Error::InvalidPointer);
	}

	let data = unsafe { std::slice::from_raw_parts(data, size) };
	Ok(data)
}
