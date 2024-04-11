#![cfg_attr(not(feature = "std"), no_std)]
#![deny(
    missing_docs,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]
#![cfg_attr(test, deny(warnings))]

//! # httparse
//!
//! A push library for parsing HTTP/1.x requests and responses.
//!
//! The focus is on speed and safety. Unsafe code is used to keep parsing fast,
//! but unsafety is contained in a submodule, with invariants enforced. The
//! parsing internals use an `Iterator` instead of direct indexing, while
//! skipping bounds checks.
//!
//! With Rust 1.27.0 or later, support for SIMD is enabled automatically.
//! If building an executable to be run on multiple platforms, and thus
//! not passing `target_feature` or `target_cpu` flags to the compiler,
//! runtime detection can still detect SSE4.2 or AVX2 support to provide
//! massive wins.
//!
//! If compiling for a specific target, remembering to include
//! `-C target_cpu=native` allows the detection to become compile time checks,
//! making it *even* faster.

use core::{fmt, result, str};
use core::mem::MaybeUninit;

use crate::iter::Bytes;

mod iter;
#[macro_use] mod macros;
mod simd;

#[doc(hidden)]
// Expose some internal functions so we can bench them individually
// WARNING: Exported for internal benchmarks, not fit for public consumption
pub mod _benchable {
    pub use super::parse_uri;
    pub use super::parse_version;
    pub use super::parse_method;
    pub use super::iter::Bytes;
}

/// Determines if byte is a token char.
///
/// > ```notrust
/// > token          = 1*tchar
/// >
/// > tchar          = "!" / "#" / "$" / "%" / "&" / "'" / "*"
/// >                / "+" / "-" / "." / "^" / "_" / "`" / "|" / "~"
/// >                / DIGIT / ALPHA
/// >                ; any VCHAR, except delimiters
/// > ```
#[inline]
fn is_token(b: u8) -> bool {
    b > 0x1F && b < 0x7F
}

// ASCII codes to accept URI string.
// i.e. A-Z a-z 0-9 !#$%&'*+-._();:@=,/?[]~^
// TODO: Make a stricter checking for URI string?
static URI_MAP: [bool; 256] = byte_map![
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
//  \0                            \n
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
//  commands
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
//  \w !  "  #  $  %  &  '  (  )  *  +  ,  -  .  /
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1,
//  0  1  2  3  4  5  6  7  8  9  :  ;  <  =  >  ?
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
//  @  A  B  C  D  E  F  G  H  I  J  K  L  M  N  O
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
//  P  Q  R  S  T  U  V  W  X  Y  Z  [  \  ]  ^  _
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
//  `  a  b  c  d  e  f  g  h  i  j  k  l  m  n  o
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0,
//  p  q  r  s  t  u  v  w  x  y  z  {  |  }  ~  del
//   ====== Extended ASCII (aka. obs-text) ======
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

#[inline]
pub(crate) fn is_uri_token(b: u8) -> bool {
    URI_MAP[b as usize]
}

static HEADER_NAME_MAP: [bool; 256] = byte_map![
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 1, 0, 1, 1, 1, 1, 1, 0, 0, 1, 1, 0, 1, 1, 0,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0,
    0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

#[inline]
pub(crate) fn is_header_name_token(b: u8) -> bool {
    HEADER_NAME_MAP[b as usize]
}

static HEADER_VALUE_MAP: [bool; 256] = byte_map![
    0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
];


#[inline]
pub(crate) fn is_header_value_token(b: u8) -> bool {
    HEADER_VALUE_MAP[b as usize]
}

/// An error in parsing.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Error {
    /// Invalid byte in header name.
    HeaderName,
    /// Invalid byte in header value.
    HeaderValue,
    /// Invalid byte in new line.
    NewLine,
    /// Invalid byte in Response status.
    Status,
    /// Invalid byte where token is required.
    Token,
    /// Parsed more headers than provided buffer can contain.
    TooManyHeaders,
    /// Invalid byte in HTTP version.
    Version,
}

impl Error {
    #[inline]
    fn description_str(&self) -> &'static str {
        match *self {
            Error::HeaderName => "invalid header name",
            Error::HeaderValue => "invalid header value",
            Error::NewLine => "invalid new line",
            Error::Status => "invalid response status",
            Error::Token => "invalid token",
            Error::TooManyHeaders => "too many headers",
            Error::Version => "invalid HTTP version",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.description_str())
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {
    fn description(&self) -> &str {
        self.description_str()
    }
}

/// An error in parsing a chunk size.
// Note: Move this into the error enum once v2.0 is released.
#[derive(Debug, PartialEq, Eq)]
pub struct InvalidChunkSize;

impl fmt::Display for InvalidChunkSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid chunk size")
    }
}

/// A Result of any parsing action.
///
/// If the input is invalid, an `Error` will be returned. Note that incomplete
/// data is not considered invalid, and so will not return an error, but rather
/// a `Ok(Status::Partial)`.
pub type Result<T> = result::Result<Status<T>, Error>;

/// The result of a successful parse pass.
///
/// `Complete` is used when the buffer contained the complete value.
/// `Partial` is used when parsing did not reach the end of the expected value,
/// but no invalid data was found.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Status<T> {
    /// The completed result.
    Complete(T),
    /// A partial result.
    Partial
}

impl<T> Status<T> {
    /// Convenience method to check if status is complete.
    #[inline]
    pub fn is_complete(&self) -> bool {
        match *self {
            Status::Complete(..) => true,
            Status::Partial => false
        }
    }

    /// Convenience method to check if status is partial.
    #[inline]
    pub fn is_partial(&self) -> bool {
        match *self {
            Status::Complete(..) => false,
            Status::Partial => true
        }
    }

    /// Convenience method to unwrap a Complete value. Panics if the status is
    /// `Partial`.
    #[inline]
    pub fn unwrap(self) -> T {
        match self {
            Status::Complete(t) => t,
            Status::Partial => panic!("Tried to unwrap Status::Partial")
        }
    }
}

/// Parser configuration.
#[derive(Clone, Debug, Default)]
pub struct ParserConfig {
    allow_spaces_after_header_name_in_responses: bool,
    allow_obsolete_multiline_headers_in_responses: bool,
    allow_multiple_spaces_in_request_line_delimiters: bool,
    allow_multiple_spaces_in_response_status_delimiters: bool,
    allow_space_before_first_header_name: bool,
    ignore_invalid_headers_in_responses: bool,
    ignore_invalid_headers_in_requests: bool,
}

impl ParserConfig {
    /// Sets whether spaces and tabs should be allowed after header names in responses.
    pub fn allow_spaces_after_header_name_in_responses(
        &mut self,
        value: bool,
    ) -> &mut Self {
        self.allow_spaces_after_header_name_in_responses = value;
        self
    }

    /// Sets whether multiple spaces are allowed as delimiters in request lines.
    ///
    /// # Background
    ///
    /// The [latest version of the HTTP/1.1 spec][spec] allows implementations to parse multiple
    /// whitespace characters in place of the `SP` delimiters in the request line, including:
    ///
    /// > SP, HTAB, VT (%x0B), FF (%x0C), or bare CR
    ///
    /// This option relaxes the parser to allow for multiple spaces, but does *not* allow the
    /// request line to contain the other mentioned whitespace characters.
    ///
    /// [spec]: https://httpwg.org/http-core/draft-ietf-httpbis-messaging-latest.html#rfc.section.3.p.3
    pub fn allow_multiple_spaces_in_request_line_delimiters(&mut self, value: bool) -> &mut Self {
        self.allow_multiple_spaces_in_request_line_delimiters = value;
        self
    }

    /// Whether multiple spaces are allowed as delimiters in request lines.
    pub fn multiple_spaces_in_request_line_delimiters_are_allowed(&self) -> bool {
        self.allow_multiple_spaces_in_request_line_delimiters
    }

    /// Sets whether multiple spaces are allowed as delimiters in response status lines.
    ///
    /// # Background
    ///
    /// The [latest version of the HTTP/1.1 spec][spec] allows implementations to parse multiple
    /// whitespace characters in place of the `SP` delimiters in the response status line,
    /// including:
    ///
    /// > SP, HTAB, VT (%x0B), FF (%x0C), or bare CR
    ///
    /// This option relaxes the parser to allow for multiple spaces, but does *not* allow the status
    /// line to contain the other mentioned whitespace characters.
    ///
    /// [spec]: https://httpwg.org/http-core/draft-ietf-httpbis-messaging-latest.html#rfc.section.4.p.3
    pub fn allow_multiple_spaces_in_response_status_delimiters(&mut self, value: bool) -> &mut Self {
        self.allow_multiple_spaces_in_response_status_delimiters = value;
        self
    }

    /// Whether multiple spaces are allowed as delimiters in response status lines.
    pub fn multiple_spaces_in_response_status_delimiters_are_allowed(&self) -> bool {
        self.allow_multiple_spaces_in_response_status_delimiters
    }

    /// Sets whether obsolete multiline headers should be allowed.
    ///
    /// This is an obsolete part of HTTP/1. Use at your own risk. If you are
    /// building an HTTP library, the newlines (`\r` and `\n`) should be
    /// replaced by spaces before handing the header value to the user.
    ///
    /// # Example
    ///
    /// ```rust
    /// let buf = b"HTTP/1.1 200 OK\r\nFolded-Header: hello\r\n there \r\n\r\n";
    /// let mut headers = [httparse::EMPTY_HEADER; 16];
    /// let mut response = httparse::Response::new(&mut headers);
    ///
    /// let res = httparse::ParserConfig::default()
    ///     .allow_obsolete_multiline_headers_in_responses(true)
    ///     .parse_response(&mut response, buf);
    ///
    /// assert_eq!(res, Ok(httparse::Status::Complete(buf.len())));
    ///
    /// assert_eq!(response.headers.len(), 1);
    /// assert_eq!(response.headers[0].name, "Folded-Header");
    /// assert_eq!(response.headers[0].value, b"hello\r\n there");
    /// ```
    pub fn allow_obsolete_multiline_headers_in_responses(
        &mut self,
        value: bool,
    ) -> &mut Self {
        self.allow_obsolete_multiline_headers_in_responses = value;
        self
    }

    /// Whether obsolete multiline headers should be allowed.
    pub fn obsolete_multiline_headers_in_responses_are_allowed(&self) -> bool {
        self.allow_obsolete_multiline_headers_in_responses
    }

    /// Sets whether white space before the first header is allowed
    ///
    /// This is not allowed by spec but some browsers ignore it. So this an option for
    /// compatibility.
    /// See https://github.com/curl/curl/issues/11605 for reference
    /// # Example
    ///
    /// ```rust
    /// let buf = b"HTTP/1.1 200 OK\r\n Space-Before-Header: hello there\r\n\r\n";
    /// let mut headers = [httparse::EMPTY_HEADER; 1];
    /// let mut response = httparse::Response::new(&mut headers[..]);
    /// let result = httparse::ParserConfig::default()
    ///     .allow_space_before_first_header_name(true)
    ///     .parse_response(&mut response, buf);

    /// assert_eq!(result, Ok(httparse::Status::Complete(buf.len())));
    /// assert_eq!(response.version.unwrap(), 1);
    /// assert_eq!(response.code.unwrap(), 200);
    /// assert_eq!(response.reason.unwrap(), "OK");
    /// assert_eq!(response.headers.len(), 1);
    /// assert_eq!(response.headers[0].name, "Space-Before-Header");
    /// assert_eq!(response.headers[0].value, &b"hello there"[..]);
    /// ```
    pub fn allow_space_before_first_header_name(&mut self, value: bool) -> &mut Self {
        self.allow_space_before_first_header_name = value;
        self
    }

    /// Whether white space before first header is allowed or not
    pub fn space_before_first_header_name_are_allowed(&self) -> bool {
        self.allow_space_before_first_header_name
    }

    /// Parses a request with the given config.
    pub fn parse_request<'buf>(
        &self,
        request: &mut Request<'_, 'buf>,
        buf: &'buf [u8],
    ) -> Result<usize> {
        request.parse_with_config(buf, self)
    }

    /// Parses a request with the given config and buffer for headers
    pub fn parse_request_with_uninit_headers<'headers, 'buf>(
        &self,
        request: &mut Request<'headers, 'buf>,
        buf: &'buf [u8],
        headers: &'headers mut [MaybeUninit<Header<'buf>>],
    ) -> Result<usize> {
        request.parse_with_config_and_uninit_headers(buf, self, headers)
    }

    /// Sets whether invalid header lines should be silently ignored in responses.
    ///
    /// This mimicks the behaviour of major browsers. You probably don't want this.
    /// You should only want this if you are implementing a proxy whose main
    /// purpose is to sit in front of browsers whose users access arbitrary content
    /// which may be malformed, and they expect everything that works without
    /// the proxy to keep working with the proxy.
    ///
    /// This option will prevent `ParserConfig::parse_response` from returning
    /// an error encountered when parsing a header, except if the error was caused
    /// by the character NUL (ASCII code 0), as Chrome specifically always reject
    /// those, or if the error was caused by a lone character `\r`, as Firefox and
    /// Chrome behave differently in that case.
    ///
    /// The ignorable errors are:
    /// * empty header names;
    /// * characters that are not allowed in header names, except for `\0` and `\r`;
    /// * when `allow_spaces_after_header_name_in_responses` is not enabled,
    ///   spaces and tabs between the header name and the colon;
    /// * missing colon between header name and value;
    /// * when `allow_obsolete_multiline_headers_in_responses` is not enabled,
    ///   headers using obsolete line folding.
    /// * characters that are not allowed in header values except for `\0` and `\r`.
    ///
    /// If an ignorable error is encountered, the parser tries to find the next
    /// line in the input to resume parsing the rest of the headers. As lines
    /// contributing to a header using obsolete line folding always start
    /// with whitespace, those will be ignored too. An error will be emitted
    /// nonetheless if it finds `\0` or a lone `\r` while looking for the
    /// next line.
    pub fn ignore_invalid_headers_in_responses(
        &mut self,
        value: bool,
    ) -> &mut Self {
        self.ignore_invalid_headers_in_responses = value;
        self
    }

    /// Sets whether invalid header lines should be silently ignored in requests.
    pub fn ignore_invalid_headers_in_requests(
        &mut self,
        value: bool,
    ) -> &mut Self {
        self.ignore_invalid_headers_in_requests = value;
        self
    }

    /// Parses a response with the given config.
    pub fn parse_response<'buf>(
        &self,
        response: &mut Response<'_, 'buf>,
        buf: &'buf [u8],
    ) -> Result<usize> {
        response.parse_with_config(buf, self)
    }

    /// Parses a response with the given config and buffer for headers
    pub fn parse_response_with_uninit_headers<'headers, 'buf>(
        &self,
        response: &mut Response<'headers, 'buf>,
        buf: &'buf [u8],
        headers: &'headers mut [MaybeUninit<Header<'buf>>],
    ) -> Result<usize> {
        response.parse_with_config_and_uninit_headers(buf, self, headers)
    }
}

/// A parsed Request.
///
/// The optional values will be `None` if a parse was not complete, and did not
/// parse the associated property. This allows you to inspect the parts that
/// could be parsed, before reading more, in case you wish to exit early.
///
/// # Example
///
/// ```
/// let buf = b"GET /404 HTTP/1.1\r\nHost:";
/// let mut headers = [httparse::EMPTY_HEADER; 16];
/// let mut req = httparse::Request::new(&mut headers);
/// let res = req.parse(buf).unwrap();
/// if res.is_partial() {
///     match req.path {
///         Some(ref path) => {
///             // check router for path.
///             // /404 doesn't exist? we could stop parsing
///         },
///         None => {
///             // must read more and parse again
///         }
///     }
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct Request<'headers, 'buf> {
    /// The request method, such as `GET`.
    pub method: Option<&'buf str>,
    /// The request path, such as `/about-us`.
    pub path: Option<&'buf str>,
    /// The request minor version, such as `1` for `HTTP/1.1`.
    pub version: Option<u8>,
    /// The request headers.
    pub headers: &'headers mut [Header<'buf>]
}

impl<'h, 'b> Request<'h, 'b> {
    /// Creates a new Request, using a slice of headers you allocate.
    #[inline]
    pub fn new(headers: &'h mut [Header<'b>]) -> Request<'h, 'b> {
        Request {
            method: None,
            path: None,
            version: None,
            headers,
        }
    }

    fn parse_with_config_and_uninit_headers(
        &mut self,
        buf: &'b [u8],
        config: &ParserConfig,
        mut headers: &'h mut [MaybeUninit<Header<'b>>],
    ) -> Result<usize> {
        let orig_len = buf.len();
        let mut bytes = Bytes::new(buf);
        complete!(skip_empty_lines(&mut bytes));
        let method = complete!(parse_method(&mut bytes));
        self.method = Some(method);
        if config.allow_multiple_spaces_in_request_line_delimiters {
            complete!(skip_spaces(&mut bytes));
        }
        self.path = Some(complete!(parse_uri(&mut bytes)));
        if config.allow_multiple_spaces_in_request_line_delimiters {
            complete!(skip_spaces(&mut bytes));
        }
        self.version = Some(complete!(parse_version(&mut bytes)));
        newline!(bytes);

        let len = orig_len - bytes.len();
        let parse_headers_status = parse_headers_iter_uninit(
            &mut headers,
            &mut bytes,
            &HeaderParserConfig {
                allow_spaces_after_header_name: false,
                allow_obsolete_multiline_headers: false,
                allow_space_before_first_header_name: config.allow_space_before_first_header_name,
                ignore_invalid_headers: config.ignore_invalid_headers_in_requests
            },
        )?;
        /* SAFETY: see `parse_headers_iter_uninit` guarantees */
        self.headers = unsafe { assume_init_slice(headers) };

        match parse_headers_status {
            Status::Complete(headers_len) => Ok(Status::Complete(len + headers_len)),
            Status::Partial => Ok(Status::Partial),
        }
    }

    /// Try to parse a buffer of bytes into the Request,
    /// except use an uninitialized slice of `Header`s.
    ///
    /// For more information, see `parse`
    pub fn parse_with_uninit_headers(
        &mut self,
        buf: &'b [u8],
        headers: &'h mut [MaybeUninit<Header<'b>>],
    ) -> Result<usize> {
        self.parse_with_config_and_uninit_headers(buf, &Default::default(), headers)
    }

    fn parse_with_config(&mut self, buf: &'b [u8], config: &ParserConfig) -> Result<usize> {
        let headers = core::mem::replace(&mut self.headers, &mut []);

        /* SAFETY: see `parse_headers_iter_uninit` guarantees */
        unsafe {
            let headers: *mut [Header<'_>] = headers;
            let headers = headers as *mut [MaybeUninit<Header<'_>>];
            match self.parse_with_config_and_uninit_headers(buf, config, &mut *headers) {
                Ok(status) => Ok(status),
                other => {
                    // put the original headers back
                    self.headers = &mut *(headers as *mut [Header<'_>]);
                    other
                },
            }
        }
    }

    /// Try to parse a buffer of bytes into the Request.
    ///
    /// Returns byte offset in `buf` to start of HTTP body.
    pub fn parse(&mut self, buf: &'b [u8]) -> Result<usize> {
        self.parse_with_config(buf, &Default::default())
    }
}

#[inline]
fn skip_empty_lines(bytes: &mut Bytes<'_>) -> Result<()> {
    loop {
        let b = bytes.peek();
        match b {
            Some(b'\r') => {
                // SAFETY: peeked and found `\r`, so it's safe to bump 1 pos
                unsafe { bytes.bump() };
                expect!(bytes.next() == b'\n' => Err(Error::NewLine));
            }
            Some(b'\n') => {
                // SAFETY: peeked and found `\n`, so it's safe to bump 1 pos
                unsafe {
                    bytes.bump();
                }
            }
            Some(..) => {
                bytes.slice();
                return Ok(Status::Complete(()));
            }
            None => return Ok(Status::Partial),
        }
    }
}

#[inline]
fn skip_spaces(bytes: &mut Bytes<'_>) -> Result<()> {
    loop {
        let b = bytes.peek();
        match b {
            Some(b' ') => {
                // SAFETY: peeked and found ` `, so it's safe to bump 1 pos
                unsafe { bytes.bump() };
            }
            Some(..) => {
                bytes.slice();
                return Ok(Status::Complete(()));
            }
            None => return Ok(Status::Partial),
        }
    }
}

/// A parsed Response.
///
/// See `Request` docs for explanation of optional values.
#[derive(Debug, Eq, PartialEq)]
pub struct Response<'headers, 'buf> {
    /// The response minor version, such as `1` for `HTTP/1.1`.
    pub version: Option<u8>,
    /// The response code, such as `200`.
    pub code: Option<u16>,
    /// The response reason-phrase, such as `OK`.
    ///
    /// Contains an empty string if the reason-phrase was missing or contained invalid characters.
    pub reason: Option<&'buf str>,
    /// The response headers.
    pub headers: &'headers mut [Header<'buf>]
}

impl<'h, 'b> Response<'h, 'b> {
    /// Creates a new `Response` using a slice of `Header`s you have allocated.
    #[inline]
    pub fn new(headers: &'h mut [Header<'b>]) -> Response<'h, 'b> {
        Response {
            version: None,
            code: None,
            reason: None,
            headers,
        }
    }

    /// Try to parse a buffer of bytes into this `Response`.
    pub fn parse(&mut self, buf: &'b [u8]) -> Result<usize> {
        self.parse_with_config(buf, &ParserConfig::default())
    }

    fn parse_with_config(&mut self, buf: &'b [u8], config: &ParserConfig) -> Result<usize> {
        let headers = core::mem::replace(&mut self.headers, &mut []);

        // SAFETY: see guarantees of [`parse_headers_iter_uninit`], which leaves no uninitialized
        // headers around. On failure, the original headers are restored.
        unsafe {
            let headers: *mut [Header<'_>] = headers;
            let headers = headers as *mut [MaybeUninit<Header<'_>>];
            match self.parse_with_config_and_uninit_headers(buf, config, &mut *headers) {
                Ok(status) => Ok(status),
                other => {
                    // put the original headers back
                    self.headers = &mut *(headers as *mut [Header<'_>]);
                    other
                },
            }
        }
    }

    fn parse_with_config_and_uninit_headers(
        &mut self,
        buf: &'b [u8],
        config: &ParserConfig,
        mut headers: &'h mut [MaybeUninit<Header<'b>>],
    ) -> Result<usize> {
        let orig_len = buf.len();
        let mut bytes = Bytes::new(buf);

        complete!(skip_empty_lines(&mut bytes));
        self.version = Some(complete!(parse_version(&mut bytes)));
        space!(bytes or Error::Version);
        if config.allow_multiple_spaces_in_response_status_delimiters {
            complete!(skip_spaces(&mut bytes));
        }
        self.code = Some(complete!(parse_code(&mut bytes)));

        // RFC7230 says there must be 'SP' and then reason-phrase, but admits
        // its only for legacy reasons. With the reason-phrase completely
        // optional (and preferred to be omitted) in HTTP2, we'll just
        // handle any response that doesn't include a reason-phrase, because
        // it's more lenient, and we don't care anyways.
        //
        // So, a SP means parse a reason-phrase.
        // A newline means go to headers.
        // Anything else we'll say is a malformed status.
        match next!(bytes) {
            b' ' => {
                if config.allow_multiple_spaces_in_response_status_delimiters {
                    complete!(skip_spaces(&mut bytes));
                }
                bytes.slice();
                self.reason = Some(complete!(parse_reason(&mut bytes)));
            },
            b'\r' => {
                expect!(bytes.next() == b'\n' => Err(Error::Status));
                bytes.slice();
                self.reason = Some("");
            },
            b'\n' => {
                bytes.slice();
                self.reason = Some("");
            }
            _ => return Err(Error::Status),
        }


        let len = orig_len - bytes.len();
        let parse_headers_status = parse_headers_iter_uninit(
            &mut headers,
            &mut bytes,
            &HeaderParserConfig {
                allow_spaces_after_header_name: config.allow_spaces_after_header_name_in_responses,
                allow_obsolete_multiline_headers: config.allow_obsolete_multiline_headers_in_responses,
                allow_space_before_first_header_name: config.allow_space_before_first_header_name,
                ignore_invalid_headers: config.ignore_invalid_headers_in_responses
            }
        )?;
        /* SAFETY: see `parse_headers_iter_uninit` guarantees */
        self.headers = unsafe { assume_init_slice(headers) };

        match parse_headers_status {
            Status::Complete(headers_len) => Ok(Status::Complete(len + headers_len)),
            Status::Partial => Ok(Status::Partial),
        }
    }
}

/// A parsed list of header fields.
///
/// The list will be populated with the header fields that could be parsed if the parse was not
/// complete. This allows you to parse headers incrementally, or inspect the parts that could be
/// parsed, before reading more, in case you wish to exit early.
///
/// # Example
///
/// ```
/// let buf = b"Host: foo.bar\nAccept: */*\n";
/// let mut headers = [httparse::EMPTY_HEADER; 16];
/// let mut headers = httparse::Headers::new(&mut headers);
/// let res = headers.parse(buf).unwrap();
/// if res.is_partial() {
///     for header in headers.headers {
///         if header.name.eq_ignore_ascii_case("accept") {
///             // Check accept header for validity.
///             // Invalid? We could stop parsing.
///         }
///     }
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct Headers<'headers, 'buf> {
    /// The headers.
    pub headers: &'headers mut [Header<'buf>]
}

impl<'h, 'b> Headers<'h, 'b> {
    /// Creates a new Headers, using a slice of headers you allocate.
    #[inline]
    pub fn new(headers: &'h mut [Header<'b>]) -> Headers<'h, 'b> {
        Headers {
            headers,
        }
    }

    fn parse_with_config_and_uninit_headers(
        &mut self,
        buf: &'b [u8],
        config: &HeaderParserConfig,
        mut headers: &'h mut [MaybeUninit<Header<'b>>],
    ) -> Result<usize> {
        let mut bytes = Bytes::new(buf);
        let parse_headers_status = parse_headers_iter_uninit(
            &mut headers,
            &mut bytes,
            &HeaderParserConfig {
                allow_spaces_after_header_name: config.allow_spaces_after_header_name,
                allow_obsolete_multiline_headers: config.allow_obsolete_multiline_headers,
                allow_space_before_first_header_name: config.allow_space_before_first_header_name,
                ignore_invalid_headers: config.ignore_invalid_headers
            },
        )?;
        /* SAFETY: see `parse_headers_iter_uninit` guarantees */
        self.headers = unsafe { assume_init_slice(headers) };

        Ok(parse_headers_status)
    }

    /// Try to parse a buffer of bytes into the Headers,
    /// except use an uninitialized slice of `Header`s.
    ///
    /// For more information, see `parse`
    pub fn parse_with_uninit_headers(
        &mut self,
        buf: &'b [u8],
        headers: &'h mut [MaybeUninit<Header<'b>>],
    ) -> Result<usize> {
        self.parse_with_config_and_uninit_headers(buf, &Default::default(), headers)
    }

    fn parse_with_config(&mut self, buf: &'b [u8], config: &HeaderParserConfig) -> Result<usize> {
        let headers = core::mem::replace(&mut self.headers, &mut []);

        /* SAFETY: see `parse_headers_iter_uninit` guarantees */
        unsafe {
            let headers: *mut [Header<'_>] = headers;
            let headers = headers as *mut [MaybeUninit<Header<'_>>];
            match self.parse_with_config_and_uninit_headers(buf, config, &mut *headers) {
                Ok(status) => Ok(status),
                other => {
                    // put the original headers back
                    self.headers = &mut *(headers as *mut [Header<'_>]);
                    other
                },
            }
        }
    }

    /// Try to parse a buffer of bytes into the Headers.
    ///
    /// Returns byte offset in `buf` to start of HTTP body.
    pub fn parse(&mut self, buf: &'b [u8]) -> Result<usize> {
        self.parse_with_config(buf, &Default::default())
    }
}

/// Represents a parsed header.
#[derive(Copy, Clone, Eq, PartialEq)]
pub struct Header<'a> {
    /// The name portion of a header.
    ///
    /// A header name must be valid ASCII-US, so it's safe to store as a `&str`.
    pub name: &'a str,
    /// The value portion of a header.
    ///
    /// While headers **should** be ASCII-US, the specification allows for
    /// values that may not be, and so the value is stored as bytes.
    pub value: &'a [u8],
}

impl<'a> fmt::Debug for Header<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_struct("Header");
        f.field("name", &self.name);
        if let Ok(value) = str::from_utf8(self.value) {
            f.field("value", &value);
        } else {
            f.field("value", &self.value);
        }
        f.finish()
    }
}

/// An empty header, useful for constructing a `Header` array to pass in for
/// parsing.
///
/// # Example
///
/// ```
/// let headers = [httparse::EMPTY_HEADER; 64];
/// ```
pub const EMPTY_HEADER: Header<'static> = Header { name: "", value: b"" };

#[inline]
#[doc(hidden)]
#[allow(missing_docs)]
// WARNING: Exported for internal benchmarks, not fit for public consumption
pub fn parse_version(bytes: &mut Bytes) -> Result<u8> {
    if let Some(eight) = bytes.peek_n::<[u8; 8]>(8) {
        // NOTE: should be const once MSRV >= 1.44
        let h10: u64 = u64::from_ne_bytes(*b"HTTP/1.0");
        let h11: u64 = u64::from_ne_bytes(*b"HTTP/1.1");
        // SAFETY: peek_n(8) before ensure within bounds
        unsafe {
            bytes.advance(8);
        }
        let block = u64::from_ne_bytes(eight);
        // NOTE: should be match once h10 & h11 are consts
        return if block == h10 {
            Ok(Status::Complete(0))
        } else if block == h11 {
            Ok(Status::Complete(1))
        } else {
            Err(Error::Version)
        };
    }

    // else (but not in `else` because of borrow checker)

    // If there aren't at least 8 bytes, we still want to detect early
    // if this is a valid version or not. If it is, we'll return Partial.
    expect!(bytes.next() == b'H' => Err(Error::Version));
    expect!(bytes.next() == b'T' => Err(Error::Version));
    expect!(bytes.next() == b'T' => Err(Error::Version));
    expect!(bytes.next() == b'P' => Err(Error::Version));
    expect!(bytes.next() == b'/' => Err(Error::Version));
    expect!(bytes.next() == b'1' => Err(Error::Version));
    expect!(bytes.next() == b'.' => Err(Error::Version));
    Ok(Status::Partial)
}

#[inline]
#[doc(hidden)]
#[allow(missing_docs)]
// WARNING: Exported for internal benchmarks, not fit for public consumption
pub fn parse_method<'a>(bytes: &mut Bytes<'a>) -> Result<&'a str> {
    const GET: [u8; 4] = *b"GET ";
    const POST: [u8; 4] = *b"POST";
    match bytes.peek_n::<[u8; 4]>(4) {
        Some(GET) => {
            // SAFETY: matched the ASCII string and boundary checked
            let method = unsafe {
                bytes.advance(4);
                let buf = bytes.slice_skip(1);
                str::from_utf8_unchecked(buf)
            };
            Ok(Status::Complete(method))
        }
        Some(POST) if bytes.peek_ahead(4) == Some(b' ') => {
            // SAFETY: matched the ASCII string and boundary checked
            let method = unsafe {
                bytes.advance(5);
                let buf = bytes.slice_skip(1);
                str::from_utf8_unchecked(buf)
            };
            Ok(Status::Complete(method))
        }
        _ => parse_token(bytes),
    }
}

/// From [RFC 7230](https://tools.ietf.org/html/rfc7230):
///
/// > ```notrust
/// > reason-phrase  = *( HTAB / SP / VCHAR / obs-text )
/// > HTAB           = %x09        ; horizontal tab
/// > VCHAR          = %x21-7E     ; visible (printing) characters
/// > obs-text       = %x80-FF
/// > ```
///
/// > A.2.  Changes from RFC 2616
/// >
/// > Non-US-ASCII content in header fields and the reason phrase
/// > has been obsoleted and made opaque (the TEXT rule was removed).
#[inline]
fn parse_reason<'a>(bytes: &mut Bytes<'a>) -> Result<&'a str> {
    let mut seen_obs_text = false;
    loop {
        let b = next!(bytes);
        if b == b'\r' {
            expect!(bytes.next() == b'\n' => Err(Error::Status));
            return Ok(Status::Complete(
                // SAFETY: (1) calling bytes.slice_skip(2) is safe, because at least two next! calls
                // advance the bytes iterator.
                // (2) calling from_utf8_unchecked is safe, because the bytes returned by slice_skip
                // were validated to be allowed US-ASCII chars by the other arms of the if/else or
                // otherwise `seen_obs_text` is true and an empty string is returned instead.
                unsafe {
                    let bytes = bytes.slice_skip(2);
                    if !seen_obs_text {
                        // all bytes up till `i` must have been HTAB / SP / VCHAR
                        str::from_utf8_unchecked(bytes)
                    } else {
                        // obs-text characters were found, so return the fallback empty string
                        ""
                    }
                },
            ));
        } else if b == b'\n' {
            return Ok(Status::Complete(
                // SAFETY: (1) calling bytes.slice_skip(1) is safe, because at least one next! call
                // advance the bytes iterator.
                // (2) see (2) of safety comment above.
                unsafe {
                    let bytes = bytes.slice_skip(1);
                    if !seen_obs_text {
                        // all bytes up till `i` must have been HTAB / SP / VCHAR
                        str::from_utf8_unchecked(bytes)
                    } else {
                        // obs-text characters were found, so return the fallback empty string
                        ""
                    }
                },
            ));
        } else if !(b == 0x09 || b == b' ' || (0x21..=0x7E).contains(&b) || b >= 0x80) {
            return Err(Error::Status);
        } else if b >= 0x80 {
            seen_obs_text = true;
        }
    }
}

#[inline]
fn parse_token<'a>(bytes: &mut Bytes<'a>) -> Result<&'a str> {
    let b = next!(bytes);
    if !is_token(b) {
        // First char must be a token char, it can't be a space which would indicate an empty token.
        return Err(Error::Token);
    }

    loop {
        let b = next!(bytes);
        if b == b' ' {
            return Ok(Status::Complete(
                // SAFETY: all bytes up till `i` must have been `is_token` and therefore also utf-8.
                unsafe { str::from_utf8_unchecked(bytes.slice_skip(1)) },
            ));
        } else if !is_token(b) {
            return Err(Error::Token);
        }
    }
}

#[inline]
#[doc(hidden)]
#[allow(missing_docs)]
// WARNING: Exported for internal benchmarks, not fit for public consumption
pub fn parse_uri<'a>(bytes: &mut Bytes<'a>) -> Result<&'a str> {
    // URI must have at least one char
    let uri_len = simd::match_uri_vectored(bytes.as_ref());
    if uri_len == 0 {
        return Err(Error::Token);
    }
    // SAFETY: these bytes have just been matched here above.
    unsafe { bytes.advance(uri_len) };
    let uri_slice = bytes.slice();

    let space_delim = next!(bytes);
    if space_delim == b' ' {
        // SAFETY: all bytes within `uri_slice` must have been `is_token` and therefore also utf-8.
        let uri = unsafe { str::from_utf8_unchecked(uri_slice) };
        Ok(Status::Complete(uri))
    } else {
        Err(Error::Token)
    }
}

#[inline]
fn parse_code(bytes: &mut Bytes<'_>) -> Result<u16> {
    let hundreds = expect!(bytes.next() == b'0'..=b'9' => Err(Error::Status));
    let tens = expect!(bytes.next() == b'0'..=b'9' => Err(Error::Status));
    let ones = expect!(bytes.next() == b'0'..=b'9' => Err(Error::Status));

    Ok(Status::Complete((hundreds - b'0') as u16 * 100 +
        (tens - b'0') as u16 * 10 +
        (ones - b'0') as u16))
}

/// Parse a buffer of bytes as headers.
///
/// The return value, if complete and successful, includes the index of the
/// buffer that parsing stopped at, and a sliced reference to the parsed
/// headers. The length of the slice will be equal to the number of properly
/// parsed headers.
///
/// # Example
///
/// ```
/// let buf = b"Host: foo.bar\nAccept: */*\n\nblah blah";
/// let mut headers = [httparse::EMPTY_HEADER; 4];
/// assert_eq!(httparse::parse_headers(buf, &mut headers),
///            Ok(httparse::Status::Complete((27, &[
///                httparse::Header { name: "Host", value: b"foo.bar" },
///                httparse::Header { name: "Accept", value: b"*/*" }
///            ][..]))));
/// ```
pub fn parse_headers<'b: 'h, 'h>(
    buf: &'b [u8],
    headers: &'h mut [Header<'b>],
) -> Result<(usize, &'h [Header<'b>])> {
    let mut headers = Headers::new(headers);
    match headers.parse(buf)? {
        Status::Complete(len) => Ok(Status::Complete((len, headers.headers))),
        Status::Partial => Ok(Status::Partial),
    }
}

unsafe fn assume_init_slice<T>(s: &mut [MaybeUninit<T>]) -> &mut [T] {
    let s: *mut [MaybeUninit<T>] = s;
    let s = s as *mut [T];
    &mut *s
}

/// Header parser configuration.
#[derive(Clone, Debug, Default)]
pub struct HeaderParserConfig {
    allow_spaces_after_header_name: bool,
    allow_obsolete_multiline_headers: bool,
    allow_space_before_first_header_name: bool,
    ignore_invalid_headers: bool,
}

impl HeaderParserConfig {
    /// Sets whether spaces and tabs should be allowed after header names.
    pub fn allow_spaces_after_header_name(
        &mut self,
        value: bool,
    ) -> &mut Self {
        self.allow_spaces_after_header_name = value;
        self
    }

    /// Whether spaces and tabs should be allowed after header names.
    pub fn spaces_after_header_name_are_allowed(&self) -> bool {
        self.allow_spaces_after_header_name
    }

    /// Sets whether obsolete multiline headers should be allowed.
    ///
    /// This is an obsolete part of HTTP/1. Use at your own risk. If you are
    /// building an HTTP library, the newlines (`\r` and `\n`) should be
    /// replaced by spaces before handing the header value to the user.
    ///
    /// # Example
    ///
    /// ```rust
    /// let buf = b"Folded-Header: hello\r\n there \r\n\r\n";
    /// let mut headers = [httparse::EMPTY_HEADER; 16];
    /// let mut headers = httparse::Headers::new(&mut headers);
    ///
    /// let res = httparse::HeaderParserConfig::default()
    ///     .allow_obsolete_multiline_headers(true)
    ///     .parse(&mut headers, buf);
    ///
    /// assert_eq!(res, Ok(httparse::Status::Complete(buf.len())));
    ///
    /// assert_eq!(headers.headers.len(), 1);
    /// assert_eq!(headers.headers[0].name, "Folded-Header");
    /// assert_eq!(headers.headers[0].value, b"hello\r\n there");
    /// ```
    pub fn allow_obsolete_multiline_headers(
        &mut self,
        value: bool,
    ) -> &mut Self {
        self.allow_obsolete_multiline_headers = value;
        self
    }

    /// Whether obsolete multiline headers should be allowed.
    pub fn obsolete_multiline_headers_are_allowed(&self) -> bool {
        self.allow_obsolete_multiline_headers
    }

    /// Sets whether white space before the first header is allowed.
    ///
    /// This is not allowed by spec but some browsers ignore it. So this an option for
    /// compatibility.
    /// See https://github.com/curl/curl/issues/11605 for reference
    /// # Example
    ///
    /// ```rust
    /// let buf = b" Space-Before-Header: hello there\r\n\r\n";
    /// let mut headers = [httparse::EMPTY_HEADER; 1];
    /// let mut headers = httparse::Headers::new(&mut headers[..]);
    /// let result = httparse::HeaderParserConfig::default()
    ///     .allow_space_before_first_header_name(true)
    ///     .parse(&mut headers, buf);

    /// assert_eq!(result, Ok(httparse::Status::Complete(buf.len())));
    /// assert_eq!(headers.headers.len(), 1);
    /// assert_eq!(headers.headers[0].name, "Space-Before-Header");
    /// assert_eq!(headers.headers[0].value, &b"hello there"[..]);
    /// ```
    pub fn allow_space_before_first_header_name(&mut self, value: bool) -> &mut Self {
        self.allow_space_before_first_header_name = value;
        self
    }

    /// Whether white space before first header is allowed or not
    pub fn space_before_first_header_name_are_allowed(&self) -> bool {
        self.allow_space_before_first_header_name
    }

    /// Sets whether invalid header lines should be silently ignored.
    ///
    /// This mimicks the behaviour of major browsers. You probably don't want this.
    /// You should only want this if you are implementing a proxy whose main
    /// purpose is to sit in front of browsers whose users access arbitrary content
    /// which may be malformed, and they expect everything that works without
    /// the proxy to keep working with the proxy.
    ///
    /// This option will prevent `HeaderParserConfig::parse` from returning
    /// an error encountered when parsing a header, except if the error was caused
    /// by the character NUL (ASCII code 0), as Chrome specifically always reject
    /// those, or if the error was caused by a lone character `\r`, as Firefox and
    /// Chrome behave differently in that case.
    ///
    /// The ignorable errors are:
    /// * empty header names;
    /// * characters that are not allowed in header names, except for `\0` and `\r`;
    /// * when `allow_spaces_after_header_name` is not enabled,
    ///   spaces and tabs between the header name and the colon;
    /// * missing colon between header name and value;
    /// * when `allow_obsolete_multiline_headers` is not enabled,
    ///   headers using obsolete line folding.
    /// * characters that are not allowed in header values except for `\0` and `\r`.
    ///
    /// If an ignorable error is encountered, the parser tries to find the next
    /// line in the input to resume parsing the rest of the headers. As lines
    /// contributing to a header using obsolete line folding always start
    /// with whitespace, those will be ignored too. An error will be emitted
    /// nonetheless if it finds `\0` or a lone `\r` while looking for the
    /// next line.
    pub fn ignore_invalid_headers(
        &mut self,
        value: bool,
    ) -> &mut Self {
        self.ignore_invalid_headers = value;
        self
    }

    /// Whether invalid header lines should be silently ignored.
    pub fn invalid_headers_are_ignored(&self) -> bool {
        self.ignore_invalid_headers
    }

    /// Parses headers with the given config.
    pub fn parse<'buf>(
        &self,
        headers: &mut Headers<'_, 'buf>,
        buf: &'buf [u8],
    ) -> Result<usize> {
        headers.parse_with_config(buf, self)
    }

    /// Parses headers with the given config and buffer
    pub fn parse_with_uninit_headers<'headers, 'buf>(
        &self,
        headers: &mut Headers<'headers, 'buf>,
        buf: &'buf [u8],
        headers_uninit: &'headers mut [MaybeUninit<Header<'buf>>],
    ) -> Result<usize> {
        headers.parse_with_config_and_uninit_headers(buf, self, headers_uninit)
    }
}

/* Function which parsers headers into uninitialized buffer.
 *
 * Guarantees that it doesn't write garbage, so casting
 * &mut &mut [Header] -> &mut &mut [MaybeUninit<Header>]
 * is safe here.
 *
 * Also it promises `headers` get shrunk to number of initialized headers,
 * so casting the other way around after calling this function is safe
 */
fn parse_headers_iter_uninit<'a>(
    headers: &mut &mut [MaybeUninit<Header<'a>>],
    bytes: &mut Bytes<'a>,
    config: &HeaderParserConfig
) -> Result<usize> {

    /* Flow of this function is pretty complex, especially with macros,
     * so this struct makes sure we shrink `headers` to only parsed ones.
     * Comparing to previous code, this only may introduce some additional
     * instructions in case of early return */
    struct ShrinkOnDrop<'r1, 'r2, 'a> {
        headers: &'r1 mut &'r2 mut [MaybeUninit<Header<'a>>],
        num_headers: usize,
    }

    impl<'r1, 'r2, 'a> Drop for ShrinkOnDrop<'r1, 'r2, 'a> {
        fn drop(&mut self) {
            let headers = core::mem::replace(self.headers, &mut []);

            /* SAFETY: num_headers is the number of initialized headers */
            let headers = unsafe { headers.get_unchecked_mut(..self.num_headers) };

            *self.headers = headers;
        }
    }

    let mut autoshrink = ShrinkOnDrop {
        headers,
        num_headers: 0,
    };
    // Track starting pointer to calculate the number of bytes parsed.
    let start = bytes.as_ref().as_ptr() as usize;
    let mut result = Err(Error::TooManyHeaders);

    let mut iter = autoshrink.headers.iter_mut();

    macro_rules! maybe_continue_after_obsolete_line_folding {
        ($bytes:ident, $label:lifetime) => {
            if config.allow_obsolete_multiline_headers {
                match $bytes.peek() {
                    None => {
                        // Next byte may be a space, in which case that header
                        // is using obsolete line folding, so we may have more
                        // whitespace to skip after colon.
                        return Ok(Status::Partial);
                    }
                    Some(b' ') | Some(b'\t') => {
                        // The space will be consumed next iteration.
                        continue $label;
                    }
                    _ => {
                        // There is another byte after the end of the line,
                        // but it's not whitespace, so it's probably another
                        // header or the final line return. This header is thus
                        // empty.
                    },
                }
            }
        }
    }

    'headers: loop {
        // Return the error `$err` if `ignore_invalid_headers_in_responses`
        // is false, otherwise find the end of the current line and resume
        // parsing on the next one.
        macro_rules! handle_invalid_char {
            ($bytes:ident, $b:ident, $err:ident) => {
                if !config.ignore_invalid_headers {
                    return Err(Error::$err);
                }

                let mut b = $b;

                loop {
                    if b == b'\r' {
                        expect!(bytes.next() == b'\n' => Err(Error::$err));
                        break;
                    }
                    if b == b'\n' {
                        break;
                    }
                    if b == b'\0' {
                        return Err(Error::$err);
                    }
                    b = next!($bytes);
                }

                $bytes.slice();

                continue 'headers;
            };
        }

        // a newline here means the head is over!
        let b = next!(bytes);
        if b == b'\r' {
            expect!(bytes.next() == b'\n' => Err(Error::NewLine));
            let end = bytes.as_ref().as_ptr() as usize;
            result = Ok(Status::Complete(end - start));
            break;
        }
        if b == b'\n' {
            let end = bytes.as_ref().as_ptr() as usize;
            result = Ok(Status::Complete(end - start));
            break;
        }
        if !is_header_name_token(b) {
            if config.allow_space_before_first_header_name
                && autoshrink.num_headers == 0
                && (b == b' ' || b == b'\t')
            {
                //advance past white space and then try parsing header again
                while let Some(peek) = bytes.peek() {
                    if peek == b' ' || peek == b'\t' {
                        next!(bytes);
                    } else {
                        break;
                    }
                }
                bytes.slice();
                continue 'headers;
            } else {
                handle_invalid_char!(bytes, b, HeaderName);
            }
        }

        #[allow(clippy::never_loop)]
        // parse header name until colon
        let header_name: &str = 'name: loop {
            let len = simd::match_header_name_vectored(bytes.as_ref());
            // SAFETY: these bytes have just been matched here above.
            unsafe { bytes.advance(len) };
            let bslice = bytes.slice();
            // SAFETY: previous call to match_header_name_vectored ensured all bytes are valid
            // header name chars, and as such also valid utf-8.
            let name = unsafe { str::from_utf8_unchecked(bslice) };

            let mut b = next!(bytes);
            if b == b':' {
                break 'name name;
            }

            if config.allow_spaces_after_header_name {
                while b == b' ' || b == b'\t' {
                    b = next!(bytes);

                    if b == b':' {
                        bytes.slice();
                        break 'name name;
                    }
                }
            }

            handle_invalid_char!(bytes, b, HeaderName);
        };

        let mut b;

        #[allow(clippy::never_loop)]
        let value_slice = 'value: loop {
            // eat white space between colon and value
            'whitespace_after_colon: loop {
                b = next!(bytes);

                if b == b' ' || b == b'\t' {
                    bytes.slice();
                    continue 'whitespace_after_colon;
                }
                if is_header_value_token(b) {
                    break 'whitespace_after_colon;
                }

                if b == b'\r' {
                    expect!(bytes.next() == b'\n' => Err(Error::HeaderValue));
                } else if b != b'\n' {
                    handle_invalid_char!(bytes, b, HeaderValue);
                }

                maybe_continue_after_obsolete_line_folding!(bytes, 'whitespace_after_colon);

                let whitespace_slice = bytes.slice();

                // This produces an empty slice that points to the beginning
                // of the whitespace.
                break 'value &whitespace_slice[0..0];
            }

            'value_lines: loop {
                // parse value till EOL

                let len = simd::match_header_value_vectored(bytes.as_ref());
                // SAFETY: these bytes have just been matched here above.
                unsafe { bytes.advance(len) };
                let b = next!(bytes);

                //found_ctl
                let skip = if b == b'\r' {
                    expect!(bytes.next() == b'\n' => Err(Error::HeaderValue));
                    2
                } else if b == b'\n' {
                    1
                } else {
                    handle_invalid_char!(bytes, b, HeaderValue);
                };

                maybe_continue_after_obsolete_line_folding!(bytes, 'value_lines);

                // SAFETY: having just checked that a newline exists, it's safe to skip it.
                unsafe {
                    break 'value bytes.slice_skip(skip);
                }
            }
        };

        let uninit_header = match iter.next() {
            Some(header) => header,
            None => break 'headers
        };

        // trim trailing whitespace in the header
        let header_value = if let Some(last_visible) = value_slice
            .iter()
            .rposition(|b| *b != b' ' && *b != b'\t' && *b != b'\r' && *b != b'\n')
        {
            // There is at least one non-whitespace character.
            &value_slice[0..last_visible+1]
        } else {
            // There is no non-whitespace character. This can only happen when value_slice is
            // empty.
            value_slice
        };

        *uninit_header = MaybeUninit::new(Header {
            name: header_name,
            value: header_value,
        });
        autoshrink.num_headers += 1;
    }

    result
}

/// Parse a buffer of bytes as a chunk size.
///
/// The return value, if complete and successful, includes the index of the
/// buffer that parsing stopped at, and the size of the following chunk.
///
/// # Example
///
/// ```
/// let buf = b"4\r\nRust\r\n0\r\n\r\n";
/// assert_eq!(httparse::parse_chunk_size(buf),
///            Ok(httparse::Status::Complete((3, 4))));
/// ```
pub fn parse_chunk_size(buf: &[u8])
    -> result::Result<Status<(usize, u64)>, InvalidChunkSize> {
    const RADIX: u64 = 16;
    let mut bytes = Bytes::new(buf);
    let mut size = 0;
    let mut in_chunk_size = true;
    let mut in_ext = false;
    let mut count = 0;
    loop {
        let b = next!(bytes);
        match b {
            b'0' ..= b'9' if in_chunk_size => {
                if count > 15 {
                    return Err(InvalidChunkSize);
                }
                count += 1;
                if cfg!(debug_assertions) && size > (core::u64::MAX / RADIX) {
                    // actually unreachable!(), because count stops the loop at 15 digits before
                    // we can reach u64::MAX / RADIX == 0xfffffffffffffff, which requires 15 hex
                    // digits. This stops mirai reporting a false alarm regarding the `size *=
                    // RADIX` multiplication below.
                    return Err(InvalidChunkSize);
                }
                size *= RADIX;
                size += (b - b'0') as u64;
            },
            b'a' ..= b'f' if in_chunk_size => {
                if count > 15 {
                    return Err(InvalidChunkSize);
                }
                count += 1;
                if cfg!(debug_assertions) && size > (core::u64::MAX / RADIX) {
                    return Err(InvalidChunkSize);
                }
                size *= RADIX;
                size += (b + 10 - b'a') as u64;
            }
            b'A' ..= b'F' if in_chunk_size => {
                if count > 15 {
                    return Err(InvalidChunkSize);
                }
                count += 1;
                if cfg!(debug_assertions) && size > (core::u64::MAX / RADIX) {
                    return Err(InvalidChunkSize);
                }
                size *= RADIX;
                size += (b + 10 - b'A') as u64;
            }
            b'\r' => {
                match next!(bytes) {
                    b'\n' => break,
                    _ => return Err(InvalidChunkSize),
                }
            }
            // If we weren't in the extension yet, the ";" signals its start
            b';' if !in_ext => {
                in_ext = true;
                in_chunk_size = false;
            }
            // "Linear white space" is ignored between the chunk size and the
            // extension separator token (";") due to the "implied *LWS rule".
            b'\t' | b' ' if !in_ext && !in_chunk_size => {}
            // LWS can follow the chunk size, but no more digits can come
            b'\t' | b' ' if in_chunk_size => in_chunk_size = false,
            // We allow any arbitrary octet once we are in the extension, since
            // they all get ignored anyway. According to the HTTP spec, valid
            // extensions would have a more strict syntax:
            //     (token ["=" (token | quoted-string)])
            // but we gain nothing by rejecting an otherwise valid chunk size.
            _ if in_ext => {}
            // Finally, if we aren't in the extension and we're reading any
            // other octet, the chunk size line is invalid!
            _ => return Err(InvalidChunkSize),
        }
    }
    Ok(Status::Complete((bytes.pos(), size)))
}

#[cfg(test)]
mod tests {
    use core::mem::MaybeUninit;
    use super::{Header, Request, Response, Status, EMPTY_HEADER, parse_chunk_size, parse_headers};

    const NUM_OF_HEADERS: usize = 4;

    macro_rules! req {
        ($name:ident, $buf:expr, |$arg:ident| $body:expr) => (
            req! {$name, $buf, Ok(Status::Complete($buf.len())), |$arg| $body }
        );
        ($name:ident, $buf:expr, $len:expr, |$arg:ident| $body:expr) => (
        #[test]
        fn $name() {
            let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
            let mut req = Request::new(&mut headers[..]);
            let status = req.parse($buf.as_ref());
            assert_eq!(status, $len);
            closure(req);

            fn closure($arg: Request) {
                $body
            }
        }
        )
    }

    req! {
        test_request_simple,
        b"GET / HTTP/1.1\r\n\r\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 0);
        }
    }

    req! {
        test_request_simple_with_query_params,
        b"GET /thing?data=a HTTP/1.1\r\n\r\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/thing?data=a");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 0);
        }
    }

    req! {
        test_request_simple_with_whatwg_query_params,
        b"GET /thing?data=a^ HTTP/1.1\r\n\r\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/thing?data=a^");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 0);
        }
    }

    req! {
        test_request_headers,
        b"GET / HTTP/1.1\r\nHost: foo.com\r\nCookie: \r\n\r\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 2);
            assert_eq!(req.headers[0].name, "Host");
            assert_eq!(req.headers[0].value, b"foo.com");
            assert_eq!(req.headers[1].name, "Cookie");
            assert_eq!(req.headers[1].value, b"");
        }
    }

    req! {
        test_request_headers_optional_whitespace,
        b"GET / HTTP/1.1\r\nHost: \tfoo.com\t \r\nCookie: \t \r\n\r\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 2);
            assert_eq!(req.headers[0].name, "Host");
            assert_eq!(req.headers[0].value, b"foo.com");
            assert_eq!(req.headers[1].name, "Cookie");
            assert_eq!(req.headers[1].value, b"");
        }
    }

    req! {
        // test the scalar parsing
        test_request_header_value_htab_short,
        b"GET / HTTP/1.1\r\nUser-Agent: some\tagent\r\n\r\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 1);
            assert_eq!(req.headers[0].name, "User-Agent");
            assert_eq!(req.headers[0].value, b"some\tagent");
        }
    }

    req! {
        // test the sse42 parsing
        test_request_header_value_htab_med,
        b"GET / HTTP/1.1\r\nUser-Agent: 1234567890some\tagent\r\n\r\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 1);
            assert_eq!(req.headers[0].name, "User-Agent");
            assert_eq!(req.headers[0].value, b"1234567890some\tagent");
        }
    }

    req! {
        // test the avx2 parsing
        test_request_header_value_htab_long,
        b"GET / HTTP/1.1\r\nUser-Agent: 1234567890some\t1234567890agent1234567890\r\n\r\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 1);
            assert_eq!(req.headers[0].name, "User-Agent");
            assert_eq!(req.headers[0].value, &b"1234567890some\t1234567890agent1234567890"[..]);
        }
    }

    req! {
        test_request_headers_max,
        b"GET / HTTP/1.1\r\nA: A\r\nB: B\r\nC: C\r\nD: D\r\n\r\n",
        |req| {
            assert_eq!(req.headers.len(), NUM_OF_HEADERS);
        }
    }

    req! {
        test_request_multibyte,
        b"GET / HTTP/1.1\r\nHost: foo.com\r\nUser-Agent: \xe3\x81\xb2\xe3/1.0\r\n\r\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 2);
            assert_eq!(req.headers[0].name, "Host");
            assert_eq!(req.headers[0].value, b"foo.com");
            assert_eq!(req.headers[1].name, "User-Agent");
            assert_eq!(req.headers[1].value, b"\xe3\x81\xb2\xe3/1.0");
        }
    }

    req! {
        test_request_partial,
        b"GET / HTTP/1.1\r\n\r", Ok(Status::Partial),
        |_req| {}
    }

    req! {
        test_request_partial_version,
        b"GET / HTTP/1.", Ok(Status::Partial),
        |_req| {}
    }

    req! {
        test_request_partial_parses_headers_as_much_as_it_can,
        b"GET / HTTP/1.1\r\nHost: yolo\r\n",
        Ok(crate::Status::Partial),
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 1);
            assert_eq!(req.headers[0].name, "Host");
            assert_eq!(req.headers[0].value, b"yolo");
        }
    }

    req! {
        test_request_newlines,
        b"GET / HTTP/1.1\nHost: foo.bar\n\n",
        |_r| {}
    }

    req! {
        test_request_empty_lines_prefix,
        b"\r\n\r\nGET / HTTP/1.1\r\n\r\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 0);
        }
    }

    req! {
        test_request_empty_lines_prefix_lf_only,
        b"\n\nGET / HTTP/1.1\n\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 0);
        }
    }

    req! {
        test_request_path_backslash,
        b"\n\nGET /\\?wayne\\=5 HTTP/1.1\n\n",
        |req| {
            assert_eq!(req.method.unwrap(), "GET");
            assert_eq!(req.path.unwrap(), "/\\?wayne\\=5");
            assert_eq!(req.version.unwrap(), 1);
            assert_eq!(req.headers.len(), 0);
        }
    }

    req! {
        test_request_with_invalid_token_delimiter,
        b"GET\n/ HTTP/1.1\r\nHost: foo.bar\r\n\r\n",
        Err(crate::Error::Token),
        |_r| {}
    }


    req! {
        test_request_with_invalid_but_short_version,
        b"GET / HTTP/1!",
        Err(crate::Error::Version),
        |_r| {}
    }

    req! {
        test_request_with_empty_method,
        b" / HTTP/1.1\r\n\r\n",
        Err(crate::Error::Token),
        |_r| {}
    }

    req! {
        test_request_with_empty_path,
        b"GET  HTTP/1.1\r\n\r\n",
        Err(crate::Error::Token),
        |_r| {}
    }

    req! {
        test_request_with_empty_method_and_path,
        b"  HTTP/1.1\r\n\r\n",
        Err(crate::Error::Token),
        |_r| {}
    }

    macro_rules! res {
        ($name:ident, $buf:expr, |$arg:ident| $body:expr) => (
            res! {$name, $buf, Ok(Status::Complete($buf.len())), |$arg| $body }
        );
        ($name:ident, $buf:expr, $len:expr, |$arg:ident| $body:expr) => (
        #[test]
        fn $name() {
            let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
            let mut res = Response::new(&mut headers[..]);
            let status = res.parse($buf.as_ref());
            assert_eq!(status, $len);
            closure(res);

            fn closure($arg: Response) {
                $body
            }
        }
        )
    }

    res! {
        test_response_simple,
        b"HTTP/1.1 200 OK\r\n\r\n",
        |res| {
            assert_eq!(res.version.unwrap(), 1);
            assert_eq!(res.code.unwrap(), 200);
            assert_eq!(res.reason.unwrap(), "OK");
        }
    }

    res! {
        test_response_newlines,
        b"HTTP/1.0 403 Forbidden\nServer: foo.bar\n\n",
        |_r| {}
    }

    res! {
        test_response_reason_missing,
        b"HTTP/1.1 200 \r\n\r\n",
        |res| {
            assert_eq!(res.version.unwrap(), 1);
            assert_eq!(res.code.unwrap(), 200);
            assert_eq!(res.reason.unwrap(), "");
        }
    }

    res! {
        test_response_reason_missing_no_space,
        b"HTTP/1.1 200\r\n\r\n",
        |res| {
            assert_eq!(res.version.unwrap(), 1);
            assert_eq!(res.code.unwrap(), 200);
            assert_eq!(res.reason.unwrap(), "");
        }
    }

    res! {
        test_response_reason_missing_no_space_with_headers,
        b"HTTP/1.1 200\r\nFoo: bar\r\n\r\n",
        |res| {
            assert_eq!(res.version.unwrap(), 1);
            assert_eq!(res.code.unwrap(), 200);
            assert_eq!(res.reason.unwrap(), "");
            assert_eq!(res.headers.len(), 1);
            assert_eq!(res.headers[0].name, "Foo");
            assert_eq!(res.headers[0].value, b"bar");
        }
    }

    res! {
        test_response_reason_with_space_and_tab,
        b"HTTP/1.1 101 Switching Protocols\t\r\n\r\n",
        |res| {
            assert_eq!(res.version.unwrap(), 1);
            assert_eq!(res.code.unwrap(), 101);
            assert_eq!(res.reason.unwrap(), "Switching Protocols\t");
        }
    }

    static RESPONSE_REASON_WITH_OBS_TEXT_BYTE: &[u8] = b"HTTP/1.1 200 X\xFFZ\r\n\r\n";
    res! {
        test_response_reason_with_obsolete_text_byte,
        RESPONSE_REASON_WITH_OBS_TEXT_BYTE,
        |res| {
            assert_eq!(res.version.unwrap(), 1);
            assert_eq!(res.code.unwrap(), 200);
            // Empty string fallback in case of obs-text
            assert_eq!(res.reason.unwrap(), "");
        }
    }

    res! {
        test_response_reason_with_nul_byte,
        b"HTTP/1.1 200 \x00\r\n\r\n",
        Err(crate::Error::Status),
        |_res| {}
    }

    res! {
        test_response_version_missing_space,
        b"HTTP/1.1",
        Ok(Status::Partial),
        |_res| {}
    }

    res! {
        test_response_code_missing_space,
        b"HTTP/1.1 200",
        Ok(Status::Partial),
        |_res| {}
    }

    res! {
        test_response_partial_parses_headers_as_much_as_it_can,
        b"HTTP/1.1 200 OK\r\nServer: yolo\r\n",
        Ok(crate::Status::Partial),
        |res| {
            assert_eq!(res.version.unwrap(), 1);
            assert_eq!(res.code.unwrap(), 200);
            assert_eq!(res.reason.unwrap(), "OK");
            assert_eq!(res.headers.len(), 1);
            assert_eq!(res.headers[0].name, "Server");
            assert_eq!(res.headers[0].value, b"yolo");
        }
    }

    res! {
        test_response_empty_lines_prefix_lf_only,
        b"\n\nHTTP/1.1 200 OK\n\n",
        |_res| {}
    }

    res! {
        test_response_no_cr,
        b"HTTP/1.0 200\nContent-type: text/html\n\n",
        |res| {
            assert_eq!(res.version.unwrap(), 0);
            assert_eq!(res.code.unwrap(), 200);
            assert_eq!(res.reason.unwrap(), "");
            assert_eq!(res.headers.len(), 1);
            assert_eq!(res.headers[0].name, "Content-type");
            assert_eq!(res.headers[0].value, b"text/html");
        }
    }

    macro_rules! headers_complete {
        ($name:ident, $buf:expr, |$arg:ident| $body:expr) => (
        #[test]
        fn $name() {
            let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
            let (_, parsed_headers) = parse_headers($buf.as_ref(), &mut headers).unwrap().unwrap();
            closure(parsed_headers);

            fn closure($arg: &[Header]) {
                $body
            }
        }
        )
    }

    headers_complete! {
        test_headers_simple,
        b"Server: foo.bar\r\n\r\n",
        |_r| {}
    }

    headers_complete! {
        test_headers,
        b"Host: foo.com\r\nCookie: \r\n\r\n",
        |headers| {
            assert_eq!(headers.len(), 2);
            assert_eq!(headers[0].name, "Host");
            assert_eq!(headers[0].value, b"foo.com");
            assert_eq!(headers[1].name, "Cookie");
            assert_eq!(headers[1].value, b"");
        }
    }

    headers_complete! {
        test_headers_newlines,
        b"Server: foo.bar\n\n",
        |_r| {}
    }

    headers_complete! {
        test_headers_no_cr,
        b"Content-type: text/html\n\n",
        |headers| {
            assert_eq!(headers.len(), 1);
            assert_eq!(headers[0].name, "Content-type");
            assert_eq!(headers[0].value, b"text/html");
        }
    }

    headers_complete! {
        test_headers_optional_whitespace,
        b"Host: \tfoo.com\t \r\nCookie: \t \r\n\r\n",
        |headers| {
            assert_eq!(headers.len(), 2);
            assert_eq!(headers[0].name, "Host");
            assert_eq!(headers[0].value, b"foo.com");
            assert_eq!(headers[1].name, "Cookie");
            assert_eq!(headers[1].value, b"");
        }
    }

    headers_complete! {
        test_headers_max,
        b"A: A\r\nB: B\r\nC: C\r\nD: D\r\n\r\n",
        |headers| {
            assert_eq!(headers.len(), NUM_OF_HEADERS);
        }
    }

    headers_complete! {
        test_headers_multibyte,
        b"Host: foo.com\r\nUser-Agent: \xe3\x81\xb2\xe3/1.0\r\n\r\n",
        |headers| {
            assert_eq!(headers.len(), 2);
            assert_eq!(headers[0].name, "Host");
            assert_eq!(headers[0].value, b"foo.com");
            assert_eq!(headers[1].name, "User-Agent");
            assert_eq!(headers[1].value, b"\xe3\x81\xb2\xe3/1.0");
        }
    }

    macro_rules! headers_partial {
        ($name:ident, $buf:expr, |$arg:ident| $body:expr) => (
        #[test]
        fn $name() {
            let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
            let status = parse_headers($buf.as_ref(), &mut headers).unwrap();
            assert_eq!(status, crate::Status::Partial);
            closure(&headers[..]);

            fn closure($arg: &[Header]) {
                $body
            }
        }
        )
    }

    headers_partial! {
        test_headers_partial_parses_headers_as_much_as_it_can,
        b"Server: yolo\r\nContent-Length: 10\r\n",
        |headers| {
            // `parse_headers` doesn't return the sliced headers in a partial parse, so we have to
            // work with the original header array, which is why the length is `NUM_OF_HEADERS`.
            assert_eq!(headers.len(), NUM_OF_HEADERS);
            assert_eq!(headers[0].name, "Server");
            assert_eq!(headers[0].value, b"yolo");
            assert_eq!(headers[1].name, "Content-Length");
            assert_eq!(headers[1].value, b"10");
        }
    }

    static RESPONSE_WITH_WHITESPACE_BETWEEN_HEADER_NAME_AND_COLON: &[u8] =
        b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Credentials : true\r\nBread: baguette\r\n\r\n";

    #[test]
    fn test_forbid_response_with_whitespace_between_header_name_and_colon() {
        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);
        let result = response.parse(RESPONSE_WITH_WHITESPACE_BETWEEN_HEADER_NAME_AND_COLON);

        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_allow_response_with_whitespace_between_header_name_and_colon() {
        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_spaces_after_header_name_in_responses(true)
            .parse_response(&mut response, RESPONSE_WITH_WHITESPACE_BETWEEN_HEADER_NAME_AND_COLON);

        assert_eq!(result, Ok(Status::Complete(77)));
        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 2);
        assert_eq!(response.headers[0].name, "Access-Control-Allow-Credentials");
        assert_eq!(response.headers[0].value, &b"true"[..]);
        assert_eq!(response.headers[1].name, "Bread");
        assert_eq!(response.headers[1].value, &b"baguette"[..]);
    }

    #[test]
    fn test_ignore_header_line_with_whitespaces_after_header_name_in_response() {
        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE_WITH_WHITESPACE_BETWEEN_HEADER_NAME_AND_COLON);

        assert_eq!(result, Ok(Status::Complete(77)));
        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Bread");
        assert_eq!(response.headers[0].value, &b"baguette"[..]);
    }

    static REQUEST_WITH_WHITESPACE_BETWEEN_HEADER_NAME_AND_COLON: &[u8] =
        b"GET / HTTP/1.1\r\nHost : localhost\r\n\r\n";

    #[test]
    fn test_forbid_request_with_whitespace_between_header_name_and_colon() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut request = Request::new(&mut headers[..]);
        let result = request.parse(REQUEST_WITH_WHITESPACE_BETWEEN_HEADER_NAME_AND_COLON);

        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_ignore_header_line_with_whitespaces_after_header_name_in_request() {
        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, REQUEST_WITH_WHITESPACE_BETWEEN_HEADER_NAME_AND_COLON);

        assert_eq!(result, Ok(Status::Complete(36)));
    }

    static RESPONSE_WITH_OBSOLETE_LINE_FOLDING_AT_START: &[u8] =
        b"HTTP/1.1 200 OK\r\nLine-Folded-Header: \r\n   \r\n hello there\r\n\r\n";

    #[test]
    fn test_forbid_response_with_obsolete_line_folding_at_start() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut response = Response::new(&mut headers[..]);
        let result = response.parse(RESPONSE_WITH_OBSOLETE_LINE_FOLDING_AT_START);

        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_allow_response_with_obsolete_line_folding_at_start() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut response = Response::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_obsolete_multiline_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE_WITH_OBSOLETE_LINE_FOLDING_AT_START);

        assert_eq!(result, Ok(Status::Complete(RESPONSE_WITH_OBSOLETE_LINE_FOLDING_AT_START.len())));
        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Line-Folded-Header");
        assert_eq!(response.headers[0].value, &b"hello there"[..]);
    }

    static RESPONSE_WITH_OBSOLETE_LINE_FOLDING_AT_END: &[u8] =
        b"HTTP/1.1 200 OK\r\nLine-Folded-Header: hello there\r\n   \r\n \r\n\r\n";

    #[test]
    fn test_forbid_response_with_obsolete_line_folding_at_end() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut response = Response::new(&mut headers[..]);
        let result = response.parse(RESPONSE_WITH_OBSOLETE_LINE_FOLDING_AT_END);

        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_allow_response_with_obsolete_line_folding_at_end() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut response = Response::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_obsolete_multiline_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE_WITH_OBSOLETE_LINE_FOLDING_AT_END);

        assert_eq!(result, Ok(Status::Complete(RESPONSE_WITH_OBSOLETE_LINE_FOLDING_AT_END.len())));
        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Line-Folded-Header");
        assert_eq!(response.headers[0].value, &b"hello there"[..]);
    }

    static RESPONSE_WITH_OBSOLETE_LINE_FOLDING_IN_MIDDLE: &[u8] =
        b"HTTP/1.1 200 OK\r\nLine-Folded-Header: hello  \r\n \r\n there\r\n\r\n";

    #[test]
    fn test_forbid_response_with_obsolete_line_folding_in_middle() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut response = Response::new(&mut headers[..]);
        let result = response.parse(RESPONSE_WITH_OBSOLETE_LINE_FOLDING_IN_MIDDLE);

        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_allow_response_with_obsolete_line_folding_in_middle() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut response = Response::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_obsolete_multiline_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE_WITH_OBSOLETE_LINE_FOLDING_IN_MIDDLE);

        assert_eq!(result, Ok(Status::Complete(RESPONSE_WITH_OBSOLETE_LINE_FOLDING_IN_MIDDLE.len())));
        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Line-Folded-Header");
        assert_eq!(response.headers[0].value, &b"hello  \r\n \r\n there"[..]);
    }

    static RESPONSE_WITH_OBSOLETE_LINE_FOLDING_IN_EMPTY_HEADER: &[u8] =
        b"HTTP/1.1 200 OK\r\nLine-Folded-Header:   \r\n \r\n \r\n\r\n";

    #[test]
    fn test_forbid_response_with_obsolete_line_folding_in_empty_header() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut response = Response::new(&mut headers[..]);
        let result = response.parse(RESPONSE_WITH_OBSOLETE_LINE_FOLDING_IN_EMPTY_HEADER);

        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_allow_response_with_obsolete_line_folding_in_empty_header() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut response = Response::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_obsolete_multiline_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE_WITH_OBSOLETE_LINE_FOLDING_IN_EMPTY_HEADER);

        assert_eq!(result, Ok(Status::Complete(RESPONSE_WITH_OBSOLETE_LINE_FOLDING_IN_EMPTY_HEADER.len())));
        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Line-Folded-Header");
        assert_eq!(response.headers[0].value, &b""[..]);
    }

    #[test]
    fn test_chunk_size() {
        assert_eq!(parse_chunk_size(b"0\r\n"), Ok(Status::Complete((3, 0))));
        assert_eq!(parse_chunk_size(b"12\r\nchunk"), Ok(Status::Complete((4, 18))));
        assert_eq!(parse_chunk_size(b"3086d\r\n"), Ok(Status::Complete((7, 198765))));
        assert_eq!(parse_chunk_size(b"3735AB1;foo bar*\r\n"), Ok(Status::Complete((18, 57891505))));
        assert_eq!(parse_chunk_size(b"3735ab1 ; baz \r\n"), Ok(Status::Complete((16, 57891505))));
        assert_eq!(parse_chunk_size(b"77a65\r"), Ok(Status::Partial));
        assert_eq!(parse_chunk_size(b"ab"), Ok(Status::Partial));
        assert_eq!(parse_chunk_size(b"567f8a\rfoo"), Err(crate::InvalidChunkSize));
        assert_eq!(parse_chunk_size(b"567f8a\rfoo"), Err(crate::InvalidChunkSize));
        assert_eq!(parse_chunk_size(b"567xf8a\r\n"), Err(crate::InvalidChunkSize));
        assert_eq!(parse_chunk_size(b"ffffffffffffffff\r\n"), Ok(Status::Complete((18, std::u64::MAX))));
        assert_eq!(parse_chunk_size(b"1ffffffffffffffff\r\n"), Err(crate::InvalidChunkSize));
        assert_eq!(parse_chunk_size(b"Affffffffffffffff\r\n"), Err(crate::InvalidChunkSize));
        assert_eq!(parse_chunk_size(b"fffffffffffffffff\r\n"), Err(crate::InvalidChunkSize));
    }

    static RESPONSE_WITH_MULTIPLE_SPACE_DELIMITERS: &[u8] =
        b"HTTP/1.1   200  OK\r\n\r\n";

    #[test]
    fn test_forbid_response_with_multiple_space_delimiters() {
        let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
        let mut response = Response::new(&mut headers[..]);
        let result = response.parse(RESPONSE_WITH_MULTIPLE_SPACE_DELIMITERS);

        assert_eq!(result, Err(crate::Error::Status));
    }

    #[test]
    fn test_allow_response_with_multiple_space_delimiters() {
        let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
        let mut response = Response::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_multiple_spaces_in_response_status_delimiters(true)
            .parse_response(&mut response, RESPONSE_WITH_MULTIPLE_SPACE_DELIMITERS);

        assert_eq!(result, Ok(Status::Complete(RESPONSE_WITH_MULTIPLE_SPACE_DELIMITERS.len())));
        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 0);
    }

    /// This is technically allowed by the spec, but we only support multiple spaces as an option,
    /// not stray `\r`s.
    static RESPONSE_WITH_WEIRD_WHITESPACE_DELIMITERS: &[u8] =
        b"HTTP/1.1 200\rOK\r\n\r\n";

    #[test]
    fn test_forbid_response_with_weird_whitespace_delimiters() {
        let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
        let mut response = Response::new(&mut headers[..]);
        let result = response.parse(RESPONSE_WITH_WEIRD_WHITESPACE_DELIMITERS);

        assert_eq!(result, Err(crate::Error::Status));
    }

    #[test]
    fn test_still_forbid_response_with_weird_whitespace_delimiters() {
        let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
        let mut response = Response::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_multiple_spaces_in_response_status_delimiters(true)
            .parse_response(&mut response, RESPONSE_WITH_WEIRD_WHITESPACE_DELIMITERS);
        assert_eq!(result, Err(crate::Error::Status));
    }

    static REQUEST_WITH_MULTIPLE_SPACE_DELIMITERS: &[u8] =
        b"GET  /    HTTP/1.1\r\n\r\n";

    #[test]
    fn test_forbid_request_with_multiple_space_delimiters() {
        let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
        let mut request = Request::new(&mut headers[..]);
        let result = request.parse(REQUEST_WITH_MULTIPLE_SPACE_DELIMITERS);

        assert_eq!(result, Err(crate::Error::Token));
    }

    #[test]
    fn test_allow_request_with_multiple_space_delimiters() {
        let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
        let mut request = Request::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_multiple_spaces_in_request_line_delimiters(true)
            .parse_request(&mut request, REQUEST_WITH_MULTIPLE_SPACE_DELIMITERS);

        assert_eq!(result, Ok(Status::Complete(REQUEST_WITH_MULTIPLE_SPACE_DELIMITERS.len())));
        assert_eq!(request.method.unwrap(), "GET");
        assert_eq!(request.path.unwrap(), "/");
        assert_eq!(request.version.unwrap(), 1);
        assert_eq!(request.headers.len(), 0);
    }

    /// This is technically allowed by the spec, but we only support multiple spaces as an option,
    /// not stray `\r`s.
    static REQUEST_WITH_WEIRD_WHITESPACE_DELIMITERS: &[u8] =
        b"GET\r/\rHTTP/1.1\r\n\r\n";

    #[test]
    fn test_forbid_request_with_weird_whitespace_delimiters() {
        let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
        let mut request = Request::new(&mut headers[..]);
        let result = request.parse(REQUEST_WITH_WEIRD_WHITESPACE_DELIMITERS);

        assert_eq!(result, Err(crate::Error::Token));
    }

    #[test]
    fn test_still_forbid_request_with_weird_whitespace_delimiters() {
        let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
        let mut request = Request::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_multiple_spaces_in_request_line_delimiters(true)
            .parse_request(&mut request, REQUEST_WITH_WEIRD_WHITESPACE_DELIMITERS);
        assert_eq!(result, Err(crate::Error::Token));
    }

    static REQUEST_WITH_MULTIPLE_SPACES_AND_BAD_PATH: &[u8] = b"GET   /foo>ohno HTTP/1.1\r\n\r\n";

    #[test]
    fn test_request_with_multiple_spaces_and_bad_path() {
        let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
        let mut request = Request::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_multiple_spaces_in_request_line_delimiters(true)
            .parse_request(&mut request, REQUEST_WITH_MULTIPLE_SPACES_AND_BAD_PATH);
        assert_eq!(result, Err(crate::Error::Token));
    }

    static RESPONSE_WITH_SPACES_IN_CODE: &[u8] = b"HTTP/1.1 99 200 OK\r\n\r\n";

    #[test]
    fn test_response_with_spaces_in_code() {
        let mut headers = [EMPTY_HEADER; NUM_OF_HEADERS];
        let mut response = Response::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_multiple_spaces_in_response_status_delimiters(true)
            .parse_response(&mut response, RESPONSE_WITH_SPACES_IN_CODE);
        assert_eq!(result, Err(crate::Error::Status));
    }

    #[test]
    fn test_response_with_empty_header_name() {
        const RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\n: hello\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .allow_spaces_after_header_name_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Ok(Status::Complete(45)));

        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Bread");
        assert_eq!(response.headers[0].value, &b"baguette"[..]);
    }

    #[test]
    fn test_request_with_empty_header_name() {
        const RESPONSE: &[u8] =
            b"GET / HTTP/1.1\r\n: hello\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_request(&mut request, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, RESPONSE);
        assert_eq!(result, Ok(Status::Complete(44)));
    }

    #[test]
    fn test_request_with_whitespace_between_header_name_and_colon() {
        const REQUEST: &[u8] =
            b"GET / HTTP/1.1\r\nAccess-Control-Allow-Credentials  : true\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .allow_spaces_after_header_name_in_responses(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()

            .ignore_invalid_headers_in_responses(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_response_with_invalid_char_between_header_name_and_colon() {
        const RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Credentials\xFF  : true\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .allow_spaces_after_header_name_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE);

        assert_eq!(result, Ok(Status::Complete(79)));
        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Bread");
        assert_eq!(response.headers[0].value, &b"baguette"[..]);
    }

    #[test]
    fn test_request_with_invalid_char_between_header_name_and_colon() {
        const REQUEST: &[u8] =
            b"GET / HTTP/1.1\r\nAccess-Control-Allow-Credentials\xFF  : true\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Ok(Status::Complete(78)));
    }

    #[test]
    fn test_ignore_header_line_with_missing_colon_in_response() {
        const RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Credentials\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Ok(Status::Complete(70)));

        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Bread");
        assert_eq!(response.headers[0].value, &b"baguette"[..]);
    }

    #[test]
    fn test_ignore_header_line_with_missing_colon_in_request() {
        const REQUEST: &[u8] =
            b"GET / HTTP/1.1\r\nAccess-Control-Allow-Credentials\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Ok(Status::Complete(69)));
    }

    #[test]
    fn test_response_header_with_missing_colon_with_folding() {
        const RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Credentials   \r\n hello\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .allow_obsolete_multiline_headers_in_responses(true)
            .allow_spaces_after_header_name_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Ok(Status::Complete(81)));

        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Bread");
        assert_eq!(response.headers[0].value, &b"baguette"[..]);
    }

    #[test]
    fn test_request_header_with_missing_colon_with_folding() {
        const REQUEST: &[u8] =
            b"GET / HTTP/1.1\r\nAccess-Control-Allow-Credentials   \r\n hello\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Ok(Status::Complete(80)));
    }

    #[test]
    fn test_response_header_with_nul_in_header_name() {
        const RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Cred\0entials: hello\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_request_header_with_nul_in_header_name() {
        const REQUEST: &[u8] =
            b"GET / HTTP/1.1\r\nAccess-Control-Allow-Cred\0entials: hello\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_header_with_cr_in_header_name() {
        const RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Cred\rentials: hello\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));

        const REQUEST: &[u8] =
            b"GET / HTTP/1.1\r\nAccess-Control-Allow-Cred\rentials: hello\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_header_with_nul_in_whitespace_before_colon() {
        const RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Credentials   \0: hello\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .allow_spaces_after_header_name_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));

        let result = crate::ParserConfig::default()
            .allow_spaces_after_header_name_in_responses(true)
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderName));

        const REQUEST: &[u8] =
            b"GET / HTTP/1.1\r\nAccess-Control-Allow-Credentials   \0: hello\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_header_with_nul_in_value() {
        const RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Credentials: hell\0o\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderValue));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderValue));

        const REQUEST: &[u8] =
            b"GET / HTTP/1.1\r\nAccess-Control-Allow-Credentials: hell\0o\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderValue));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderValue));
    }

    #[test]
    fn test_header_with_invalid_char_in_value() {
        const RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Credentials: hell\x01o\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderValue));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Ok(Status::Complete(78)));

        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Bread");
        assert_eq!(response.headers[0].value, &b"baguette"[..]);

        const REQUEST: &[u8] =
            b"GET / HTTP/1.1\r\nAccess-Control-Allow-Credentials: hell\x01o\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderValue));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Ok(Status::Complete(77)));

        assert_eq!(request.version.unwrap(), 1);
        assert_eq!(request.method.unwrap(), "GET");
        assert_eq!(request.path.unwrap(), "/");
        assert_eq!(request.headers.len(), 1);
        assert_eq!(request.headers[0].name, "Bread");
        assert_eq!(request.headers[0].value, &b"baguette"[..]);
    }

    #[test]
    fn test_header_with_invalid_char_in_value_with_folding() {
        const RESPONSE: &[u8] =
            b"HTTP/1.1 200 OK\r\nAccess-Control-Allow-Credentials: hell\x01o  \n world!\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut response = Response::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Err(crate::Error::HeaderValue));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_responses(true)
            .parse_response(&mut response, RESPONSE);
        assert_eq!(result, Ok(Status::Complete(88)));

        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Bread");
        assert_eq!(response.headers[0].value, &b"baguette"[..]);

        const REQUEST: &[u8] =
            b"GET / HTTP/1.1\r\nAccess-Control-Allow-Credentials: hell\x01o  \n world!\r\nBread: baguette\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 2];
        let mut request = Request::new(&mut headers[..]);

        let result = crate::ParserConfig::default()
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Err(crate::Error::HeaderValue));

        let result = crate::ParserConfig::default()
            .ignore_invalid_headers_in_requests(true)
            .parse_request(&mut request, REQUEST);
        assert_eq!(result, Ok(Status::Complete(87)));

        assert_eq!(request.version.unwrap(), 1);
        assert_eq!(request.method.unwrap(), "GET");
        assert_eq!(request.path.unwrap(), "/");
        assert_eq!(request.headers.len(), 1);
        assert_eq!(request.headers[0].name, "Bread");
        assert_eq!(request.headers[0].value, &b"baguette"[..]);
    }

    #[test]
    fn test_method_within_buffer() {
        const REQUEST: &[u8] = b"GET / HTTP/1.1\r\n\r\n";

        let mut headers = [EMPTY_HEADER; 0];
        let mut request = Request::new(&mut headers[..]);

        crate::ParserConfig::default()
            .parse_request(&mut request, REQUEST)
            .unwrap();

        // SAFETY: will not wrap
        let buf_end = unsafe { REQUEST.as_ptr().add(REQUEST.len()) };
        // Check that the method str is within the buffer
        let method = request.method.unwrap();
        assert!(REQUEST.as_ptr() <= method.as_ptr());
        assert!(method.as_ptr() <= buf_end);
    }

     static RESPONSE_WITH_SPACE_BEFORE_FIRST_HEADER: &[u8] =
        b"HTTP/1.1 200 OK\r\n Space-Before-Header: hello there\r\n\r\n";

    #[test]
    fn test_forbid_response_with_space_before_first_header() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut response = Response::new(&mut headers[..]);
        let result = response.parse(RESPONSE_WITH_SPACE_BEFORE_FIRST_HEADER);

        assert_eq!(result, Err(crate::Error::HeaderName));
    }

    #[test]
    fn test_allow_response_response_with_space_before_first_header() {
        let mut headers = [EMPTY_HEADER; 1];
        let mut response = Response::new(&mut headers[..]);
        let result = crate::ParserConfig::default()
            .allow_space_before_first_header_name(true)
            .parse_response(&mut response, RESPONSE_WITH_SPACE_BEFORE_FIRST_HEADER);

        assert_eq!(
            result,
            Ok(Status::Complete(
                RESPONSE_WITH_SPACE_BEFORE_FIRST_HEADER.len()
            ))
        );
        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 1);
        assert_eq!(response.headers[0].name, "Space-Before-Header");
        assert_eq!(response.headers[0].value, &b"hello there"[..]);
    }

    #[test]
    fn test_request_partial_with_uninit_headers() {
        const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nFoo: bar\r\nBaz: quux\r\n";

        let mut headers = unsafe {
            MaybeUninit::<[MaybeUninit<crate::Header<'_>>; 4]>::uninit().assume_init()
        };

        let mut request = Request::new(&mut []);

        let result = crate::ParserConfig::default()
            .parse_request_with_uninit_headers(&mut request, REQUEST, &mut headers);

        assert_eq!(result, Ok(Status::Partial));
        assert_eq!(request.method.unwrap(), "GET");
        assert_eq!(request.path.unwrap(), "/");
        assert_eq!(request.version.unwrap(), 1);
        assert_eq!(request.headers.len(), 2);
        assert_eq!(request.headers[0].name, "Foo");
        assert_eq!(request.headers[0].value, &b"bar"[..]);
        assert_eq!(request.headers[1].name, "Baz");
        assert_eq!(request.headers[1].value, &b"quux"[..]);
    }

    #[test]
    fn test_response_partial_with_uninit_headers() {
        const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nFoo: bar\r\nBaz: quux\r\n";

        let mut headers = unsafe {
            MaybeUninit::<[MaybeUninit<crate::Header<'_>>; 4]>::uninit().assume_init()
        };

        let mut response = Response::new(&mut []);

        let result = crate::ParserConfig::default()
            .parse_response_with_uninit_headers(&mut response, RESPONSE, &mut headers);

        assert_eq!(result, Ok(Status::Partial));
        assert_eq!(response.version.unwrap(), 1);
        assert_eq!(response.code.unwrap(), 200);
        assert_eq!(response.reason.unwrap(), "OK");
        assert_eq!(response.headers.len(), 2);
        assert_eq!(response.headers[0].name, "Foo");
        assert_eq!(response.headers[0].value, &b"bar"[..]);
        assert_eq!(response.headers[1].name, "Baz");
        assert_eq!(response.headers[1].value, &b"quux"[..]);
    }

    #[test]
    fn test_headers_partial_with_uninit_headers() {
        const HEADERS: &[u8] = b"Foo: bar\r\nBaz: quux\r\n";

        let mut headers_uninit = unsafe {
            MaybeUninit::<[MaybeUninit<crate::Header<'_>>; 4]>::uninit().assume_init()
        };

        let mut headers = crate::Headers::new(&mut []);

        let result = headers.parse_with_uninit_headers(HEADERS, &mut headers_uninit);

        assert_eq!(result, Ok(Status::Partial));
        assert_eq!(headers.headers.len(), 2);
        assert_eq!(headers.headers[0].name, "Foo");
        assert_eq!(headers.headers[0].value, &b"bar"[..]);
        assert_eq!(headers.headers[1].name, "Baz");
        assert_eq!(headers.headers[1].value, &b"quux"[..]);
    }
}
