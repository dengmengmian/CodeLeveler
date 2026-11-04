//! Raw CoreFoundation / CoreGraphics access, kept in one place.
//!
//! These bindings are declarations of public OS interfaces, not a copied
//! implementation. Every pointer is either owned (released by `CfOwned`) or
//! borrowed for the duration of one call. Nothing here crosses a thread.

use std::ffi::{c_char, c_void};

pub(crate) type CFTypeRef = *const c_void;
pub(crate) type CFStringRef = *const c_void;
pub(crate) type CFArrayRef = *const c_void;
pub(crate) type CFDictionaryRef = *const c_void;
pub(crate) type CFNumberRef = *const c_void;
pub(crate) type CFBooleanRef = *const c_void;
pub(crate) type CFIndex = isize;
pub(crate) type CFTypeID = usize;
pub(crate) type Boolean = u8;
pub(crate) type AXUIElementRef = *const c_void;

pub(crate) const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
pub(crate) const CF_NUMBER_DOUBLE: i32 = 13;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn CFRelease(value: CFTypeRef);
    pub(crate) fn CFRetain(value: CFTypeRef) -> CFTypeRef;
    pub(crate) fn CFGetTypeID(value: CFTypeRef) -> CFTypeID;

    pub(crate) fn CFStringCreateWithCString(
        allocator: *const c_void,
        string: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    pub(crate) fn CFStringGetCString(
        string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> Boolean;
    pub(crate) fn CFStringGetLength(string: CFStringRef) -> CFIndex;
    pub(crate) fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    pub(crate) fn CFStringGetTypeID() -> CFTypeID;

    pub(crate) fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
    pub(crate) fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> *const c_void;
    pub(crate) fn CFArrayGetTypeID() -> CFTypeID;

    pub(crate) fn CFDictionaryGetValue(
        dictionary: CFDictionaryRef,
        key: *const c_void,
    ) -> *const c_void;
    pub(crate) fn CFDictionaryGetTypeID() -> CFTypeID;

    pub(crate) fn CFNumberGetValue(
        number: CFNumberRef,
        number_type: i32,
        value: *mut c_void,
    ) -> Boolean;
    pub(crate) fn CFNumberGetTypeID() -> CFTypeID;

    pub(crate) fn CFBooleanGetValue(boolean: CFBooleanRef) -> Boolean;
    pub(crate) fn CFBooleanGetTypeID() -> CFTypeID;

    pub(crate) static kCFBooleanTrue: CFBooleanRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn AXIsProcessTrusted() -> Boolean;
    pub(crate) fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    pub(crate) fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    pub(crate) fn AXUIElementCopyActionNames(
        element: AXUIElementRef,
        names: *mut CFArrayRef,
    ) -> i32;
    pub(crate) fn AXUIElementPerformAction(element: AXUIElementRef, action: CFStringRef) -> i32;
    pub(crate) fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    pub(crate) fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout: f32) -> i32;
    pub(crate) fn AXValueGetType(value: CFTypeRef) -> u32;
    pub(crate) fn AXValueGetValue(value: CFTypeRef, value_type: u32, out: *mut c_void) -> Boolean;
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CGPointRaw {
    pub(crate) x: f64,
    pub(crate) y: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CGSizeRaw {
    pub(crate) width: f64,
    pub(crate) height: f64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CGRectRaw {
    pub(crate) origin: CGPointRaw,
    pub(crate) size: CGSizeRaw,
}

pub(crate) const AX_VALUE_CG_POINT: u32 = 1;
pub(crate) const AX_VALUE_CG_SIZE: u32 = 2;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    pub(crate) fn CGPreflightScreenCaptureAccess() -> Boolean;
    pub(crate) fn CGSessionCopyCurrentDictionary() -> CFDictionaryRef;
    pub(crate) fn CGMainDisplayID() -> u32;
    pub(crate) fn CGDisplayBounds(display: u32) -> CGRectRaw;
    pub(crate) fn CGDisplayPixelsWide(display: u32) -> usize;
    pub(crate) fn CGWindowListCopyWindowInfo(option: u32, relative: u32) -> CFArrayRef;
    pub(crate) fn CGEventCreateKeyboardEvent(
        source: CFTypeRef,
        virtual_key: u16,
        key_down: Boolean,
    ) -> CFTypeRef;
    pub(crate) fn CGEventKeyboardSetUnicodeString(
        event: CFTypeRef,
        length: usize,
        string: *const u16,
    );
    pub(crate) fn CGEventSetFlags(event: CFTypeRef, flags: u64);
    pub(crate) fn CGEventCreateMouseEvent(
        source: CFTypeRef,
        mouse_type: u32,
        x: f64,
        y: f64,
        button: u32,
    ) -> CFTypeRef;
    pub(crate) fn CGEventSetIntegerValueField(event: CFTypeRef, field: u32, value: i64);
    pub(crate) fn CGEventSetLocation(event: CFTypeRef, point: CGPointRaw);
    pub(crate) fn CGEventCreateScrollWheelEvent2(
        source: CFTypeRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CFTypeRef;
    pub(crate) fn CGEventPost(tap: u32, event: CFTypeRef);
}

/// A CoreFoundation value this call owns. It is released exactly once, on the
/// worker thread that created it.
pub(crate) struct CfOwned(CFTypeRef);

impl CfOwned {
    /// Wrap a value returned under the create rule.
    pub(crate) unsafe fn from_create(value: CFTypeRef) -> Option<Self> {
        (!value.is_null()).then_some(Self(value))
    }

    /// Retain a borrowed value so it can outlive its container.
    pub(crate) fn retain(value: CFTypeRef) -> Option<Self> {
        if value.is_null() {
            None
        } else {
            unsafe {
                CFRetain(value);
            }
            Some(Self(value))
        }
    }

    pub(crate) fn as_ptr(&self) -> CFTypeRef {
        self.0
    }

    pub(crate) fn type_id(&self) -> CFTypeID {
        unsafe { CFGetTypeID(self.0) }
    }

    pub(crate) fn is_string(&self) -> bool {
        self.type_id() == unsafe { CFStringGetTypeID() }
    }

    pub(crate) fn is_array(&self) -> bool {
        self.type_id() == unsafe { CFArrayGetTypeID() }
    }

    pub(crate) fn is_dictionary(&self) -> bool {
        self.type_id() == unsafe { CFDictionaryGetTypeID() }
    }

    pub(crate) fn is_number(&self) -> bool {
        self.type_id() == unsafe { CFNumberGetTypeID() }
    }

    pub(crate) fn is_boolean(&self) -> bool {
        self.type_id() == unsafe { CFBooleanGetTypeID() }
    }

    pub(crate) fn as_string(&self) -> Option<String> {
        if !self.is_string() {
            return None;
        }
        let string = self.0 as CFStringRef;
        let length = unsafe { CFStringGetLength(string) };
        let capacity =
            unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8) } + 1;
        if capacity <= 0 {
            return Some(String::new());
        }
        let mut buffer = vec![0u8; capacity as usize];
        let ok = unsafe {
            CFStringGetCString(
                string,
                buffer.as_mut_ptr() as *mut c_char,
                buffer.len() as CFIndex,
                CF_STRING_ENCODING_UTF8,
            )
        };
        if ok == 0 {
            return None;
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        Some(String::from_utf8_lossy(&buffer[..end]).into_owned())
    }

    pub(crate) fn as_double(&self) -> Option<f64> {
        if !self.is_number() {
            return None;
        }
        let mut value = 0f64;
        let ok = unsafe {
            CFNumberGetValue(
                self.0 as CFNumberRef,
                CF_NUMBER_DOUBLE,
                &mut value as *mut f64 as *mut c_void,
            )
        };
        (ok != 0).then_some(value)
    }

    pub(crate) fn as_i64(&self) -> Option<i64> {
        self.as_double().map(|value| value as i64)
    }

    pub(crate) fn as_bool(&self) -> Option<bool> {
        if !self.is_boolean() {
            return None;
        }
        Some(unsafe { CFBooleanGetValue(self.0 as CFBooleanRef) } != 0)
    }
}

impl Drop for CfOwned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

/// A CFString owned by this call, for use as an attribute name or dictionary
/// key. The caller keeps it alive for the call.
pub(crate) struct CfString(CfOwned);

impl CfString {
    pub(crate) fn new(value: &str) -> Option<Self> {
        let cstring = std::ffi::CString::new(value).ok()?;
        let raw = unsafe {
            CFStringCreateWithCString(std::ptr::null(), cstring.as_ptr(), CF_STRING_ENCODING_UTF8)
        };
        unsafe { CfOwned::from_create(raw) }.map(Self)
    }

    pub(crate) fn as_ptr(&self) -> CFStringRef {
        self.0.as_ptr() as CFStringRef
    }

    /// The value as an untyped reference, for attributes that accept a
    /// CFString (for example `AXValue`).
    pub(crate) fn as_value(&self) -> CFTypeRef {
        self.0.as_ptr()
    }
}

pub(crate) fn array_count(array: &CfOwned) -> usize {
    if !array.is_array() {
        return 0;
    }
    unsafe { CFArrayGetCount(array.as_ptr() as CFArrayRef) }.max(0) as usize
}

pub(crate) fn array_value(array: &CfOwned, index: usize) -> *const c_void {
    unsafe { CFArrayGetValueAtIndex(array.as_ptr() as CFArrayRef, index as CFIndex) }
}

pub(crate) fn dictionary_value(dictionary: &CfOwned, key: &str) -> Option<CfOwned> {
    if !dictionary.is_dictionary() {
        return None;
    }
    let key = CfString::new(key)?;
    let value = unsafe {
        CFDictionaryGetValue(
            dictionary.as_ptr() as CFDictionaryRef,
            key.as_ptr() as *const c_void,
        )
    };
    CfOwned::retain(value)
}
