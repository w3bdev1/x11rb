//! A FFI-based connection to an X11 server, using libxcb.

use std::ptr::{null, null_mut};
use std::convert::{TryFrom, TryInto};
use std::ffi::CStr;
use std::io::IoSlice;
use std::ops::Deref;
#[cfg(unix)]
use std::os::unix::io::{AsRawFd, RawFd};
use libc::c_void;
use crate::utils::{CSlice, Buffer, RawFdContainer};
use crate::x11_utils::{GenericError, GenericEvent, Event};
use crate::errors::{ParseError, ConnectionError, ConnectionErrorOrX11Error};
use crate::connection::{Connection, VoidCookie, Cookie, CookieWithFds, SequenceNumber, ExtensionInformation, RequestKind, DiscardMode};
use super::generated::xproto::{Setup, QueryExtensionReply};

/// A connection to an X11 server.
///
/// This type wraps `*mut xcb_connection_t` that is provided by libxcb. It provides a rust
/// interface to this C library.
#[derive(Debug)]
pub struct XCBConnection {
    conn: raw_ffi::XCBConnectionWrapper,
    setup: Setup,
    ext_info: ExtensionInformation,
    errors: pending_errors::PendingErrors
}

mod pending_errors {
    use std::cmp::Reverse;
    use std::sync::Mutex;
    use std::convert::TryInto;
    use std::collections::{BinaryHeap, VecDeque};

    use super::XCBConnection;
    use crate::connection::SequenceNumber;
    use crate::x11_utils::GenericError;

    #[derive(Debug, Default)]
    struct PendingErrorsInner {
        in_flight: BinaryHeap<Reverse<SequenceNumber>>,
        pending: VecDeque<GenericError>,
    }

    /// A management struct for pending X11 errors
    #[derive(Debug, Default)]
    pub(crate) struct PendingErrors {
        inner: Mutex<PendingErrorsInner>
    }

    impl PendingErrors {
        pub(crate) fn append_error(&self, error: GenericError) {
            self.inner.lock().unwrap().pending.push_back(error)
        }

        pub(crate) fn discard_reply(&self, sequence: SequenceNumber) {
            self.inner.lock().unwrap().in_flight.push(Reverse(sequence));
        }

        pub(crate) fn get(&self, conn: &XCBConnection) -> Option<GenericError> {
            let mut inner = self.inner.lock().unwrap();

            // Check if we already have an element at hand
            let err = inner.pending.pop_front();
            if err.is_some() {
                return err;
            }

            // Check if any of the still in-flight responses got a reply/error
            while let Some(Reverse(seqno)) = inner.in_flight.peek() {
                let result = match conn.poll_for_reply(*seqno) {
                    Err(()) => {
                        // This request was not answered/errored yet, so later request will not
                        // have answers as well.
                        return None;
                    },
                    Ok(reply) => reply
                };

                let seqno = *seqno;
                std::mem::forget(seqno);
                let seqno2 = inner.in_flight.pop();
                assert_eq!(Some(Reverse(seqno)), seqno2);

                if let Some(result) = result {
                    // Is this an error?
                    if let Ok(error) = result.try_into() {
                        return Some(error);
                    } else {
                        // It's a reply, just ignore it
                    }
                }
            }

            None
        }
    }
}

impl XCBConnection {
    unsafe fn connection_error_from_connection(c: *const raw_ffi::xcb_connection_t) -> ConnectionError {
        Self::connection_error_from_c_error(raw_ffi::xcb_connection_has_error(c))
    }

    fn connection_error_from_c_error(error: i32) -> ConnectionError {
        use crate::xcb_ffi::raw_ffi::connection_errors::*;

        assert_ne!(error, 0);
        match error {
            ERROR => ConnectionError::ConnectionError,
            EXT_NOTSUPPORTED => ConnectionError::UnsupportedExtension,
            MEM_INSUFFICIENT => ConnectionError::InsufficientMemory,
            REQ_LEN_EXCEED => ConnectionError::MaximumRequestLengthExceeded,
            PARSE_ERR => ConnectionError::DisplayParsingError,
            INVALID_SCREEN => ConnectionError::InvalidScreen,
            FDPASSING_FAILED => ConnectionError::FDPassingFailed,
            _ => ConnectionError::UnknownError
        }
    }

    /// Establish a new connection to an X11 server.
    ///
    /// If a `dpy_name` is provided, it describes the display that should be connected to, for
    /// example `127.0.0.1:1`. If no value is provided, the `$DISPLAY` environment variable is
    /// used.
    pub fn connect(dpy_name: Option<&CStr>) -> Result<(XCBConnection, usize), ConnectionError>  {
        use libc::c_int;
        unsafe {
            let mut screen: c_int = 0;
            let dpy_ptr = dpy_name.map_or(null(), |s| s.as_ptr());
            let connection = raw_ffi::xcb_connect(dpy_ptr, &mut screen);
            let error = raw_ffi::xcb_connection_has_error(connection);
            if error != 0 {
                raw_ffi::xcb_disconnect(connection);
                Err(Self::connection_error_from_c_error(error.try_into().or(Err(ConnectionError::UnknownError))?))
            } else {
                let setup = raw_ffi::xcb_get_setup(connection);
                let conn = XCBConnection {
                    conn: raw_ffi::XCBConnectionWrapper(connection),
                    setup: Self::parse_setup(setup)?,
                    ext_info: Default::default(),
                    errors: Default::default()
                };
                Ok((conn, screen as usize))
            }
        }
    }

    unsafe fn parse_setup(setup: *const u8) -> Result<Setup, ParseError> {
        use std::slice::from_raw_parts;

        // We know that the setup information has at least eight bytes.
        // Use a slice instead of Buffer::CSlice since we must not free() the xcb_setup_t that libxcb owns.
        let wrapper = from_raw_parts(setup, 8);

        // The length field is in the last two bytes
        let length = u16::from_ne_bytes([wrapper[6], wrapper[7]]);

        // The length is in four-byte-units after the known header
        let length = length * 4 + 8;

        let slice = from_raw_parts(wrapper.as_ptr(), length.try_into()?);
        let result = Setup::try_from(&*slice)?;

        Ok(result)
    }

    fn send_request(&self, bufs: &[IoSlice], fds: Vec<RawFdContainer>, has_reply: bool, reply_has_fds: bool) -> Result<SequenceNumber, ConnectionError> {
        // For this, we derefence the IoSlices, add two new entries, and create new IoSlices.
        let mut new_bufs = Vec::with_capacity(2 + bufs.len());

        // XCB wants to access bufs[-1] and bufs[-2], so we need to add two empty items in front.
        new_bufs.push(&[][..]);
        new_bufs.push(&[][..]);

        // Add the actual request buffers
        let mut storage = Default::default();
        new_bufs.extend(self.compute_length_field(bufs, &mut storage)?.iter().map(Deref::deref));

        // Now wrap the buffers with IoSlice
        let mut new_bufs = new_bufs.into_iter().map(IoSlice::new).collect::<Vec<_>>();

        // Set up the information that libxcb needs
        let protocol_request = raw_ffi::xcb_protocol_request_t {
            count: bufs.len(),
            ext: null_mut(), // Not needed since we always use raw
            opcode: 0,
            isvoid: if has_reply { 0 } else { 1 }
        };
        let mut flags = raw_ffi::send_request_flags::RAW;
        assert!(has_reply || !reply_has_fds);
        flags |= raw_ffi::send_request_flags::CHECKED;
        if reply_has_fds {
            flags |= raw_ffi::send_request_flags::REPLY_FDS;
        }

        // Convert the FDs into an array of ints. libxcb will close the FDs.
        let fds: Vec<_> = fds.into_iter().map(RawFdContainer::into_raw_fd).collect();

        let seqno = if fds.is_empty() {
            unsafe { raw_ffi::xcb_send_request64((self.conn).0, flags, &mut new_bufs[2], &protocol_request) }
        } else {
            let num_fds = fds.len().try_into().unwrap();
            let fds_ptr = fds.as_ptr();
            unsafe { raw_ffi::xcb_send_request_with_fds64((self.conn).0, flags, &mut new_bufs[2], &protocol_request, num_fds, fds_ptr) }
        };
        if seqno == 0 {
            unsafe { Err(XCBConnection::connection_error_from_connection((self.conn).0)) }
        } else {
            Ok(seqno)
        }
    }

    /// Check if the underlying XCB connection is in an error state.
    pub fn has_error(&self) -> Option<ConnectionError> {
        unsafe {
            let error = raw_ffi::xcb_connection_has_error((self.conn).0);
            if error == 0 {
                None
            } else {
                Some(Self::connection_error_from_c_error(error))
            }
        }
    }

    /// Get access to the raw libxcb `xcb_connection_t`.
    ///
    /// The returned pointer is valid for as long as the original object was not dropped. No
    /// ownerhsip is transferred.
    pub fn get_raw_xcb_connection(&self) -> *mut c_void {
        (self.conn).0 as _
    }

    /// Check if a reply to the given request already received.
    ///
    /// Return Err(()) when the reply was not yet received. Returns Ok(None) when there can be no
    /// reply. Returns Ok(buffer) with the reply if there is one (this buffer can be an error or a
    /// reply).
    fn poll_for_reply(&self, sequence: SequenceNumber) -> Result<Option<Buffer>, ()> {
        unsafe {
            let mut reply = null_mut();
            let mut error = null_mut();
            let found = raw_ffi::xcb_poll_for_reply((self.conn).0, sequence as _, &mut reply, &mut error);
            if found == 0 {
                return Err(());
            }
            assert_eq!(found, 1);
            match (reply == null_mut(), error == null_mut()) {
                (true, true) => Ok(None),
                (true, false) => Ok(Some(XCBConnection::wrap_error(error as _))),
                (false, true) => Ok(Some(XCBConnection::wrap_reply(reply as _))),
                (false, false) => unreachable!()
            }
        }
    }

    unsafe fn wrap_reply(reply: *const u8) -> Buffer {
        let header = CSlice::new(reply, 32);

        let length_field = u32::from_ne_bytes(header[4..8].try_into().unwrap());
        let length_field: usize = length_field.try_into()
            .expect("usize should have at least 32 bits");

        let length = 32 + length_field * 4;
        Buffer::from_raw_parts(header.into_ptr(), length)
    }

    unsafe fn wrap_error(error: *const u8) -> Buffer {
         Buffer::from_raw_parts(error, 32)
    }
}

impl Connection for XCBConnection {
    fn send_request_with_reply<R>(&self, bufs: &[IoSlice], fds: Vec<RawFdContainer>) -> Result<Cookie<Self, R>, ConnectionError>
        where R: TryFrom<Buffer, Error=ParseError>
    {
        Ok(Cookie::new(self, self.send_request(bufs, fds, true, false)?))
    }

    fn send_request_with_reply_with_fds<R>(&self, bufs: &[IoSlice], fds: Vec<RawFdContainer>) -> Result<CookieWithFds<Self, R>, ConnectionError>
        where R: TryFrom<(Buffer, Vec<RawFdContainer>), Error=ParseError>
    {
        Ok(CookieWithFds::new(self, self.send_request(bufs, fds, true, true)?))
    }

    fn send_request_without_reply(&self, bufs: &[IoSlice], fds: Vec<RawFdContainer>) -> Result<VoidCookie<Self>, ConnectionError> {
        Ok(VoidCookie::new(self, self.send_request(bufs, fds, false, false)?))
    }

    fn discard_reply(&self, sequence: SequenceNumber, _kind: RequestKind, mode: DiscardMode) {
        match mode {
            DiscardMode::DiscardReplyAndError => unsafe {
                // libxcb can throw away everything for us
                raw_ffi::xcb_discard_reply64((self.conn).0, sequence);
            },
            // We have to check for errors ourselves
            DiscardMode::DiscardReply => self.errors.discard_reply(sequence)
        }
    }

    fn extension_information(&self, extension_name: &'static str) -> Option<&QueryExtensionReply> {
        self.ext_info.extension_information(self, extension_name)
    }

    fn wait_for_reply_or_error(&self, sequence: SequenceNumber) -> Result<Buffer, ConnectionErrorOrX11Error> {
        unsafe {
            let mut error = null_mut();
            let reply = raw_ffi::xcb_wait_for_reply64((self.conn).0, sequence, &mut error);

            // At least one of these pointers must be NULL.
            assert!(reply == null_mut() || error == null_mut());

            // If both pointers are NULL, the xcb connection must be in an error state
            if reply == null_mut() && error == null_mut() {
                Err(Self::connection_error_from_connection((self.conn).0))?
            }

            if reply != null_mut() {
                Ok(XCBConnection::wrap_reply(reply as _))
            } else {
                let error: GenericError = XCBConnection::wrap_error(error as _).try_into()?;
                Err(error.into())
            }
        }
    }

    fn wait_for_reply(&self, sequence: SequenceNumber) -> Result<Option<Buffer>, ConnectionError> {
        use ConnectionErrorOrX11Error::*;
        match self.wait_for_reply_or_error(sequence) {
            Ok(buffer) => Ok(Some(buffer)),
            Err(err) => match err {
                ConnectionError(err) => Err(err),
                X11Error(err) => {
                    self.errors.append_error(err.into());
                    Ok(None)
                }
            }
        }
    }

    fn check_for_error(&self, sequence: SequenceNumber) -> Result<Option<GenericError>, ConnectionError> {
        let cookie = raw_ffi::xcb_void_cookie_t { sequence: sequence as _ };
        let error = unsafe { raw_ffi::xcb_request_check((self.conn).0, cookie) };
        if error == null_mut() {
            Ok(None)
        } else {
            unsafe { Ok(Some(Buffer::from_raw_parts(error as _, 32).try_into()?)) }
        }
    }

    #[cfg(unix)]
    fn wait_for_reply_with_fds(&self, sequence: SequenceNumber) -> Result<(Buffer, Vec<RawFdContainer>), ConnectionErrorOrX11Error> {
        let buffer = self.wait_for_reply_or_error(sequence)?;

        // Get a pointer to the array of integers where libxcb saved the FD numbers
        let fd_ptr = match &buffer {
            &Buffer::Vec(_) => unreachable!(), // wait_for_reply() always returns a CSlice
            &Buffer::CSlice(ref slice) => {
                // libxcb saves the list of FDs after the data of the reply
                (unsafe { slice.as_ptr().add(slice.len()) }) as *const RawFd
            }
        };

        // The number of FDs is in the second byte (= buffer[1]) in all replies.
        let vector = unsafe { std::slice::from_raw_parts(fd_ptr, buffer[1] as usize) };
        let vector = vector.iter().map(|&fd| RawFdContainer::new(fd)).collect();

        Ok((buffer, vector))
    }

    #[cfg(not(unix))]
    fn wait_for_reply_with_fds(&self, _sequence: SequenceNumber) -> Result<(Buffer, Vec<RawFdContainer>), ConnectionErrorOrX11Error> {
        unimplemented!("FD passing is currently only implemented on Unix-like systems")
    }

    fn wait_for_event(&self) -> Result<GenericEvent, ConnectionError> {
        if let Some(error) = self.errors.get(self) {
            return Ok(error.into());
        }
        unsafe {
            let event = raw_ffi::xcb_wait_for_event((self.conn).0);
            if event.is_null() {
                return Err(Self::connection_error_from_connection((self.conn).0));
            }
            let generic_event: GenericEvent = Buffer::from_raw_parts(event as _, 32).try_into()?;
            assert_ne!(35, generic_event.response_type()); // FIXME: XGE events may have sizes > 32
            Ok(generic_event)
        }
    }

    fn poll_for_event(&self) -> Result<Option<GenericEvent>, ConnectionError> {
        if let Some(error) = self.errors.get(self) {
            return Ok(Some(error.into()));
        }
        unsafe {
            let event = raw_ffi::xcb_poll_for_event((self.conn).0);
            if event.is_null() {
                let err = raw_ffi::xcb_connection_has_error((self.conn).0);
                if err == 0 {
                    return Ok(None);
                } else {
                    return Err(Self::connection_error_from_c_error(err));
                }
            }
            let generic_event: GenericEvent = Buffer::from_raw_parts(event as _, 32).try_into()?;
            assert_ne!(35, generic_event.response_type()); // FIXME: XGE events may have sizes > 32
            Ok(Some(generic_event))
        }
    }

    fn flush(&self) {
        // xcb_flush() returns 0 if the connection is in (or just entered) an error state, else 1.
        // Adding a Result<(), ConnectionError> as a return value here would be too noisy, I think,
        // so just ignore this return value.
        let _ = unsafe { raw_ffi::xcb_flush((self.conn).0) };
    }

    fn generate_id(&self) -> u32 {
        unsafe { raw_ffi::xcb_generate_id((self.conn).0) }
    }

    fn setup(&self) -> &Setup {
        &self.setup
    }

    fn maximum_request_bytes(&self) -> usize {
        4 * unsafe { raw_ffi::xcb_get_maximum_request_length((self.conn).0) as usize }
    }
}

impl Drop for XCBConnection {
    fn drop(&mut self) {
        unsafe {
            raw_ffi::xcb_disconnect((self.conn).0 as *mut raw_ffi::xcb_connection_t);
        }
    }
}

#[cfg(unix)]
impl AsRawFd for XCBConnection {
    fn as_raw_fd(&self) -> RawFd {
        unsafe {
            raw_ffi::xcb_get_file_descriptor((self.conn).0)
        }
    }
}

mod raw_ffi {
    #[cfg(not(test))]
    use std::io::IoSlice;
    use libc::{c_int, c_char, c_uint};
    #[cfg(not(test))]
    use libc::c_void;

    #[allow(non_camel_case_types)]
    #[repr(C)]
    pub(crate) struct xcb_connection_t {
        _unused: [u8; 0]
    }

    #[derive(Debug)]
    pub(crate) struct XCBConnectionWrapper(pub(crate) *const xcb_connection_t);

    // libxcb is fully thread-safe (well, except for xcb_disconnect()), so the following is
    // actually fine and safe:
    unsafe impl Send for XCBConnectionWrapper {}
    unsafe impl Sync for XCBConnectionWrapper {}

    #[allow(non_camel_case_types)]
    #[repr(C)]
    pub(crate) struct xcb_extension_t {
        pub(crate) name: *const c_char,
        pub(crate) global_id: c_int
    }

    #[allow(non_camel_case_types)]
    #[repr(C)]
    pub(crate) struct xcb_void_cookie_t {
        pub(crate) sequence: c_uint
    }

    #[allow(non_camel_case_types)]
    #[repr(C)]
    pub(crate) struct xcb_protocol_request_t {
        pub(crate) count: usize,
        pub(crate) ext: *mut xcb_extension_t,
        pub(crate) opcode: u8,
        pub(crate) isvoid: u8
    }

    pub(crate) mod connection_errors {
        pub(crate) const ERROR: i32 = 1;
        pub(crate) const EXT_NOTSUPPORTED: i32 = 2;
        pub(crate) const MEM_INSUFFICIENT: i32 = 3;
        pub(crate) const REQ_LEN_EXCEED: i32 = 4;
        pub(crate) const PARSE_ERR: i32 = 5;
        pub(crate) const INVALID_SCREEN: i32 = 6;
        pub(crate) const FDPASSING_FAILED: i32 = 7;
    }

    pub(crate) mod send_request_flags {
        use libc::c_int;

        pub(crate) const CHECKED: c_int = 1;
        pub(crate) const RAW: c_int = 2;
        //pub(crate) const DISCARD_REPLY: c_int = 4;
        pub(crate) const REPLY_FDS: c_int = 8;
    }

    #[cfg(not(test))]
    #[link(name = "xcb")]
    extern {
        pub(crate) fn xcb_connect(displayname: *const c_char, screenp: *mut c_int ) -> *mut xcb_connection_t;
        pub(crate) fn xcb_disconnect(c: *mut xcb_connection_t);
        pub(crate) fn xcb_connection_has_error(c: *const xcb_connection_t) -> c_int;
        pub(crate) fn xcb_send_request64(c: *const xcb_connection_t, flags: c_int, vector: *mut IoSlice, request: *const xcb_protocol_request_t) -> u64;
        pub(crate) fn xcb_send_request_with_fds64(c: *const xcb_connection_t, flags: c_int, vector: *mut IoSlice, request: *const xcb_protocol_request_t, num_fds: c_uint, fds: *const c_int) -> u64;
        pub(crate) fn xcb_discard_reply64(c: *const xcb_connection_t, sequence: u64);
        pub(crate) fn xcb_wait_for_reply64(c: *const xcb_connection_t, request: u64, e: *mut *mut c_void) -> *mut c_void;
        pub(crate) fn xcb_poll_for_reply(c: *const xcb_connection_t, request: c_uint, reply: *mut *mut c_void, e: *mut *mut c_void) -> c_int;
        pub(crate) fn xcb_request_check(c: *const xcb_connection_t, void_cookie: xcb_void_cookie_t) -> *mut c_void;
        pub(crate) fn xcb_wait_for_event(c: *const xcb_connection_t) -> *mut c_void;
        pub(crate) fn xcb_poll_for_event(c: *const xcb_connection_t) -> *mut c_void;
        pub(crate) fn xcb_flush(c: *const xcb_connection_t) -> c_int;
        pub(crate) fn xcb_generate_id(c: *const xcb_connection_t) -> u32;
        pub(crate) fn xcb_get_setup(c: *const xcb_connection_t) -> *const u8;
        #[cfg(unix)]
        pub(crate) fn xcb_get_file_descriptor(c: *const xcb_connection_t) -> c_int;
        pub(crate) fn xcb_get_maximum_request_length(c: *const xcb_connection_t) -> u32;
    }

    #[cfg(test)]
    mod mock {
        use std::io::IoSlice;
        use std::ffi::CStr;
        use std::cell::RefCell;
        use libc::{c_void, c_int, c_char, c_uint};
        use super::{xcb_connection_t, xcb_protocol_request_t, xcb_void_cookie_t};

        thread_local!(pub(crate) static SETUP_BYTES: RefCell<Option<Vec<u8>>> = RefCell::new(None));

        #[repr(C)]
        struct ConnectionMock {
            xcb_conn: xcb_connection_t,
            error: c_int,
            setup: *const u8,
        }

        pub(crate) unsafe fn xcb_connect(displayname: *const c_char, screenp: *mut c_int ) -> *mut xcb_connection_t {
            // Test that the provided displayname is correct
            if CStr::from_ptr(displayname).to_str().unwrap() != "display name" {
                panic!("Did not get the expected displayname");
            }
            std::ptr::write(screenp, 0);
            let mock = ConnectionMock {
                xcb_conn: xcb_connection_t { _unused: Default::default() },
                error: 0,
                setup: SETUP_BYTES.with(|f| f.borrow().as_ref().unwrap().as_ptr()),
            };
            Box::into_raw(Box::new(mock)) as _
        }

        pub(crate) unsafe fn xcb_disconnect(c: *mut xcb_connection_t) {
            let _ = Box::from_raw(c);
        }

        pub(crate) unsafe fn xcb_connection_has_error(c: *const xcb_connection_t) -> c_int {
            std::ptr::read(c as *const ConnectionMock).error
        }

        pub(crate) unsafe fn xcb_send_request64(_c: *const xcb_connection_t, _flags: c_int, _vector: *mut IoSlice, _request: *const xcb_protocol_request_t) -> u64 {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_send_request_with_fds64(_c: *const xcb_connection_t, _flags: c_int, _vector: *mut IoSlice, _request: *const xcb_protocol_request_t, _num_fds: c_uint, _fds: *const c_int) -> u64 {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_discard_reply64(_c: *const xcb_connection_t, _sequence: u64) {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_wait_for_reply64(_c: *const xcb_connection_t, _request: u64, _e: *mut *mut c_void) -> *mut c_void {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_poll_for_reply(_c: *const xcb_connection_t, _request: c_uint, _reply: *mut *mut c_void, _e: *mut *mut c_void) -> c_int {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_request_check(_c: *const xcb_connection_t, _void_cookie: xcb_void_cookie_t) -> *mut c_void {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_wait_for_event(_c: *const xcb_connection_t) -> *mut c_void {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_poll_for_event(_c: *const xcb_connection_t) -> *mut c_void {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_flush(_c: *const xcb_connection_t) -> c_int {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_generate_id(_c: *const xcb_connection_t) -> u32 {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_get_setup(c: *const xcb_connection_t) -> *const u8 {
            std::ptr::read(c as *mut ConnectionMock).setup
        }

        #[cfg(unix)]
        pub(crate) unsafe fn xcb_get_file_descriptor(_c: *const xcb_connection_t) -> c_int {
            unimplemented!();
        }

        pub(crate) unsafe fn xcb_get_maximum_request_length(_c: *const xcb_connection_t) -> u32 {
            unimplemented!();
        }
    }

    #[cfg(test)]
    pub(crate) use mock::*;
}

#[cfg(test)]
mod test {
    use std::ffi::CString;
    use super::{XCBConnection, ConnectionError, Connection};
    use super::raw_ffi::SETUP_BYTES;

    fn default_setup() {
        let mut s = Vec::new();
        // 58
        let vendor_len: u16 = 2;
        let num_pixmap_formats: u8 = 1;
        let roots_len: u8 = 18;
        let header: u16 = 10;
        let length: u16 = header + vendor_len + 2 * num_pixmap_formats as u16 + roots_len as u16;

        s.extend(&[1, 0]); // Status "success" and padding
        s.extend(&11u16.to_ne_bytes()); // major version
        s.extend(&0u16.to_ne_bytes()); // minor version
        s.extend(&length.to_ne_bytes()); // length
        s.extend(&0x12345678u32.to_ne_bytes()); // release number
        s.extend(&0x10000000u32.to_ne_bytes()); // resource id base
        s.extend(&0x000000ffu32.to_ne_bytes()); // resource id mask
        s.extend(&0u32.to_ne_bytes()); // motion buffer size
        s.extend(&6u16.to_ne_bytes()); // vendor length
        s.extend(&0x100u16.to_ne_bytes()); // maximum request length
        s.push(1); // roots length
        s.push(num_pixmap_formats); // pixmap formats length
        s.push(1); // image byte order: MSB first
        s.push(1); // bitmap format bit order: MSB first
        s.push(0); // scanline unit
        s.push(0); // scanline pad
        s.push(0); // min keycode
        s.push(0xff); // max keycode
        s.extend(&[0, 0, 0, 0]); // padding
        assert_eq!(s.len(), header as usize * 4);

        s.extend("Vendor  ".bytes()); // vendor + padding
        assert_eq!(s.len(), (header + vendor_len) as usize * 4);

        // Pixmap formats, we said above there is one entry
        s.push(15); // depth
        s.push(42); // bits per pixel
        s.push(21); // scanline pad
        s.extend(&[0, 0, 0, 0, 0]); // padding
        assert_eq!(s.len(), (header + vendor_len + 2 * num_pixmap_formats as u16) as usize * 4);

        // Screens, we said above there is one entry
        s.extend(&1u32.to_ne_bytes()); // root window
        s.extend(&2u32.to_ne_bytes()); // default colormap
        s.extend(&3u32.to_ne_bytes()); // white pixel
        s.extend(&4u32.to_ne_bytes()); // black pixel
        s.extend(&0u32.to_ne_bytes()); // current input masks
        s.extend(&0u16.to_ne_bytes()); // width in pixels
        s.extend(&0u16.to_ne_bytes()); // height in pixels
        s.extend(&0u16.to_ne_bytes()); // width in mm
        s.extend(&0u16.to_ne_bytes()); // height in mm
        s.extend(&0u16.to_ne_bytes()); // min installed maps
        s.extend(&0u16.to_ne_bytes()); // max installed maps
        s.extend(&0u32.to_ne_bytes()); // root visual
        s.extend(&[0, 0, 0, 1]); // backing stores, save unders, root depths, allowed depths len

        // one depth entry
        s.extend(&[99, 0]); // depth and padding
        s.extend(&1u16.to_ne_bytes()); // width visuals len
        s.extend(&[0, 0, 0, 0]); // padding

        // one visualtype entry
        s.extend(&80u32.to_ne_bytes()); // visualid
        s.extend(&[2, 4]); // class and bits per rgb value
        s.extend(&81u16.to_ne_bytes()); // colormap entries
        s.extend(&82u32.to_ne_bytes()); // red mask
        s.extend(&83u32.to_ne_bytes()); // green mask
        s.extend(&84u32.to_ne_bytes()); // blue mask
        s.extend(&[0, 0, 0, 0]); // padding

        assert_eq!(s.len(), length as usize * 4);

        s.extend(std::iter::repeat(0).take(1000)); // padding
        SETUP_BYTES.with(|f| *f.borrow_mut() = Some(s));
    }

    #[test]
    fn xcb_connect_and_setup() -> Result<(), ConnectionError> {
        default_setup();
        let str = CString::new("display name").unwrap();
        let (conn, screen) = XCBConnection::connect(Some(&str)).expect("Failed to 'connect'");
        assert_eq!(screen, 0);

        let setup = &conn.setup();
        assert_eq!((1, 11, 0), (setup.status, setup.protocol_major_version, setup.protocol_minor_version));
        assert_eq!(0x12345678, setup.release_number);
        assert_eq!((0, 0xff), (setup.min_keycode, setup.max_keycode));
        assert_eq!("Vendor".as_bytes(), &setup.vendor[..]);

        assert_eq!(1, setup.pixmap_formats.len());
        let format = &setup.pixmap_formats[0];
        assert_eq!(15, format.depth);
        assert_eq!(42, format.bits_per_pixel);
        assert_eq!(21, format.scanline_pad);

        assert_eq!(1, setup.roots.len());
        let root = &setup.roots[0];
        assert_eq!((1, 2, 3, 4), (root.root, root.default_colormap, root.white_pixel, root.black_pixel));

        assert_eq!(1, root.allowed_depths.len());
        let depth = &root.allowed_depths[0];
        assert_eq!(99, depth.depth);

        assert_eq!(1, depth.visuals.len());
        let visual = &depth.visuals[0];
        assert_eq!(80, visual.visual_id);
        assert_eq!(2, visual.class);
        assert_eq!(4, visual.bits_per_rgb_value);
        assert_eq!(81, visual.colormap_entries);
        assert_eq!(82, visual.red_mask);
        assert_eq!(83, visual.green_mask);
        assert_eq!(84, visual.blue_mask);

        Ok(())
    }
}
