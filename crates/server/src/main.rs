use azookey_server::TonicNamedPipeServer;
use tonic::{transport::Server, Request, Response, Status};
use tonic_reflection::server::Builder as ReflectionBuilder;

use shared::proto::azookey_service_server::{AzookeyService, AzookeyServiceServer};
use shared::proto::{
    AppendTextRequest, AppendTextResponse, ClearTextRequest, ClearTextResponse,
    CommitPrefixAndAppendRequest, CommitPrefixAndAppendResponse, ComposingText, MoveCursorRequest,
    MoveCursorResponse, RemoveTextRequest, RemoveTextResponse, ShrinkTextRequest,
    ShrinkTextResponse, Suggestion,
};

use std::collections::HashSet;
use std::ffi::{c_char, CStr, CString};
use std::fmt;
use std::sync::{Mutex, MutexGuard};

const USE_ZENZAI: bool = true;
const FFI_SUCCESS: i32 = 0;

struct RawComposingText {
    text: String,
}

#[derive(Debug)]
enum FfiError {
    InteriorNul(&'static str),
    CallFailed(&'static str),
    Status {
        operation: &'static str,
        status: i32,
    },
    InvalidCandidateCount(i32),
    NullCandidate(usize),
    NullCandidateField {
        index: usize,
        field: &'static str,
    },
    LockPoisoned,
}

impl fmt::Display for FfiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InteriorNul(field) => write!(formatter, "{field} contains an interior NUL byte"),
            Self::CallFailed(operation) => write!(formatter, "{operation} returned a null result"),
            Self::Status { operation, status } => {
                write!(formatter, "{operation} failed with FFI status {status}")
            }
            Self::InvalidCandidateCount(count) => {
                write!(
                    formatter,
                    "GetComposedText returned invalid candidate count {count}"
                )
            }
            Self::NullCandidate(index) => {
                write!(
                    formatter,
                    "GetComposedText returned a null candidate at index {index}"
                )
            }
            Self::NullCandidateField { index, field } => write!(
                formatter,
                "GetComposedText returned a null {field} pointer at index {index}"
            ),
            Self::LockPoisoned => formatter.write_str("Swift FFI lock is poisoned"),
        }
    }
}

impl std::error::Error for FfiError {}

fn ffi_error_status(error: FfiError) -> Status {
    if matches!(&error, FfiError::InteriorNul(_)) {
        Status::invalid_argument(error.to_string())
    } else {
        Status::internal(error.to_string())
    }
}

#[derive(Debug)]
#[repr(C)]
struct FFICandidate {
    text: *mut c_char,
    subtext: *mut c_char,
    corresponding_count: i32,
}

unsafe extern "C" {
    fn Initialize(path: *const c_char, use_zenzai: i32) -> i32;
    fn SetContext(context: *const c_char) -> i32;
    fn AppendText(input: *const c_char, cursor: *mut i32) -> *mut c_char;
    fn RemoveText(cursor: *mut i32) -> *mut c_char;
    fn MoveCursor(offset: i32, cursor: *mut i32) -> *mut c_char;
    fn ShrinkText(offset: i32) -> *mut c_char;
    fn CommitPrefixAndAppend(offset: i32, input: *const c_char, cursor: *mut i32) -> *mut c_char;
    fn ClearText();
    fn GetComposedText(length: *mut i32) -> *mut *mut FFICandidate;
    fn GetRawInput() -> *mut c_char;
    fn FreeCString(string: *mut c_char);
    fn FreeCandidateList(candidates: *mut *mut FFICandidate, length: i32);
    fn LoadConfig() -> i32;
}

struct OwnedFfiCString(*mut c_char);

impl OwnedFfiCString {
    fn new(pointer: *mut c_char, operation: &'static str) -> Result<Self, FfiError> {
        if pointer.is_null() {
            Err(FfiError::CallFailed(operation))
        } else {
            Ok(Self(pointer))
        }
    }

    fn to_string_lossy(&self) -> String {
        // SAFETY: The Swift FFI returns a non-null, NUL-terminated allocation and keeps it
        // alive until this guard is dropped.
        unsafe { CStr::from_ptr(self.0).to_string_lossy().into_owned() }
    }
}

impl Drop for OwnedFfiCString {
    fn drop(&mut self) {
        // SAFETY: This guard has unique ownership of a string returned by the Swift FFI.
        unsafe { FreeCString(self.0) };
    }
}

struct OwnedCandidateList {
    pointer: *mut *mut FFICandidate,
    length: i32,
}

impl Drop for OwnedCandidateList {
    fn drop(&mut self) {
        if !self.pointer.is_null() && self.length >= 0 {
            // SAFETY: The pointer and length are the unchanged pair returned by
            // GetComposedText, and this guard is their unique owner.
            unsafe { FreeCandidateList(self.pointer, self.length) };
        }
    }
}

fn make_c_string(value: &str, field: &'static str) -> Result<CString, FfiError> {
    CString::new(value).map_err(|_| FfiError::InteriorNul(field))
}

fn check_status(operation: &'static str, status: i32) -> Result<(), FfiError> {
    if status == FFI_SUCCESS {
        Ok(())
    } else {
        Err(FfiError::Status { operation, status })
    }
}

fn copy_owned_string(pointer: *mut c_char, operation: &'static str) -> Result<String, FfiError> {
    Ok(OwnedFfiCString::new(pointer, operation)?.to_string_lossy())
}

fn initialize(path: &str) -> Result<(), FfiError> {
    let path = make_c_string(path, "executable path")?;
    // SAFETY: path is NUL-terminated and valid for the duration of the call.
    let status = unsafe { Initialize(path.as_ptr(), if USE_ZENZAI { 1 } else { 0 }) };
    check_status("Initialize", status)
}

fn add_text(input: &str) -> Result<RawComposingText, FfiError> {
    let input = make_c_string(input, "text_to_append")?;
    let mut cursor = 0;
    // SAFETY: input and cursor are valid for the duration of the call.
    let result = unsafe { AppendText(input.as_ptr(), &mut cursor) };
    Ok(RawComposingText {
        text: copy_owned_string(result, "AppendText")?,
    })
}

fn move_cursor(offset: i32) -> Result<RawComposingText, FfiError> {
    let mut cursor = 0;
    // SAFETY: cursor is a valid output pointer.
    let result = unsafe { MoveCursor(offset, &mut cursor) };
    Ok(RawComposingText {
        text: copy_owned_string(result, "MoveCursor")?,
    })
}

fn remove_text() -> Result<RawComposingText, FfiError> {
    let mut cursor = 0;
    // SAFETY: cursor is a valid output pointer.
    let result = unsafe { RemoveText(&mut cursor) };
    Ok(RawComposingText {
        text: copy_owned_string(result, "RemoveText")?,
    })
}

fn clear_text() {
    // SAFETY: ClearText takes no arguments and returns no owned data.
    unsafe { ClearText() };
}

fn candidate_string(
    pointer: *mut c_char,
    index: usize,
    field: &'static str,
) -> Result<String, FfiError> {
    if pointer.is_null() {
        return Err(FfiError::NullCandidateField { index, field });
    }
    // SAFETY: The candidate-list owner keeps all candidate strings alive. Swift creates each
    // field with strdup, so it is NUL-terminated.
    Ok(unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned())
}

fn candidate_list_count(pointer: *mut *mut FFICandidate, length: i32) -> Result<usize, FfiError> {
    if length < 0 {
        return Err(FfiError::InvalidCandidateCount(length));
    }
    if length == 0 {
        return Ok(0);
    }
    if pointer.is_null() {
        return Err(FfiError::CallFailed("GetComposedText"));
    }

    let count = usize::try_from(length).map_err(|_| FfiError::InvalidCandidateCount(length))?;
    let byte_length = count
        .checked_mul(std::mem::size_of::<*mut FFICandidate>())
        .ok_or(FfiError::InvalidCandidateCount(length))?;
    if byte_length > isize::MAX as usize {
        return Err(FfiError::InvalidCandidateCount(length));
    }
    Ok(count)
}

fn get_composed_text() -> Result<Vec<Suggestion>, FfiError> {
    let mut length = -1;
    // SAFETY: length is a valid output pointer. The returned allocation is immediately placed
    // in an ownership guard.
    let pointer = unsafe { GetComposedText(&mut length) };
    let result = OwnedCandidateList { pointer, length };

    let count = candidate_list_count(pointer, length)?;
    if count == 0 {
        return Ok(Vec::new());
    }

    let mut suggestions = Vec::with_capacity(count);
    let mut seen_texts = HashSet::with_capacity(count);
    for index in 0..count {
        // SAFETY: Swift allocated exactly `length` initialized pointer slots.
        let candidate_pointer = unsafe { *result.pointer.add(index) };
        if candidate_pointer.is_null() {
            return Err(FfiError::NullCandidate(index));
        }
        // SAFETY: Null was checked above and the list owner keeps the allocation alive.
        let candidate = unsafe { &*candidate_pointer };
        let text = candidate_string(candidate.text, index, "text")?;
        if !seen_texts.insert(text.clone()) {
            continue;
        }
        let subtext = candidate_string(candidate.subtext, index, "subtext")?;
        suggestions.push(Suggestion {
            text,
            subtext,
            corresponding_count: candidate.corresponding_count,
        });
    }

    Ok(suggestions)
}

fn get_raw_input() -> Result<String, FfiError> {
    // SAFETY: GetRawInput takes no arguments and returns a string owned by the Swift FFI.
    let result = unsafe { GetRawInput() };
    copy_owned_string(result, "GetRawInput")
}

fn shrink_text(offset: i32) -> Result<RawComposingText, FfiError> {
    // SAFETY: ShrinkText takes a fixed-width integer and returns an owned C string.
    let result = unsafe { ShrinkText(offset) };
    Ok(RawComposingText {
        text: copy_owned_string(result, "ShrinkText")?,
    })
}

fn commit_prefix_and_append(offset: i32, input: &str) -> Result<RawComposingText, FfiError> {
    let input = make_c_string(input, "text_to_append")?;
    let mut cursor = 0;
    // SAFETY: input is NUL-terminated and cursor is a valid output pointer.
    let result = unsafe { CommitPrefixAndAppend(offset, input.as_ptr(), &mut cursor) };
    Ok(RawComposingText {
        text: copy_owned_string(result, "CommitPrefixAndAppend")?,
    })
}

fn convert(raw: RawComposingText) -> Result<ComposingText, FfiError> {
    let raw_input = get_raw_input()?;
    let suggestions = get_composed_text()?;
    Ok(ComposingText {
        hiragana: raw.text,
        suggestions,
        raw_input,
    })
}

#[derive(Debug, Default)]
pub struct MyAzookeyService {
    ffi_lock: Mutex<()>,
}

impl MyAzookeyService {
    fn lock_ffi(&self) -> Result<MutexGuard<'_, ()>, FfiError> {
        self.ffi_lock.lock().map_err(|_| FfiError::LockPoisoned)
    }
}

#[tonic::async_trait]
impl AzookeyService for MyAzookeyService {
    async fn append_text(
        &self,
        request: Request<AppendTextRequest>,
    ) -> Result<Response<AppendTextResponse>, Status> {
        let _ffi_guard = self.lock_ffi().map_err(ffi_error_status)?;
        let input = request.into_inner().text_to_append;
        let composing_text = add_text(&input)
            .and_then(convert)
            .map_err(ffi_error_status)?;

        Ok(Response::new(AppendTextResponse {
            composing_text: Some(composing_text),
        }))
    }

    async fn remove_text(
        &self,
        _: Request<RemoveTextRequest>,
    ) -> Result<Response<RemoveTextResponse>, Status> {
        let _ffi_guard = self.lock_ffi().map_err(ffi_error_status)?;
        let composing_text = remove_text().and_then(convert).map_err(ffi_error_status)?;

        Ok(Response::new(RemoveTextResponse {
            composing_text: Some(composing_text),
        }))
    }

    async fn move_cursor(
        &self,
        request: Request<MoveCursorRequest>,
    ) -> Result<Response<MoveCursorResponse>, Status> {
        let _ffi_guard = self.lock_ffi().map_err(ffi_error_status)?;
        let composing_text = move_cursor(request.into_inner().offset)
            .and_then(convert)
            .map_err(ffi_error_status)?;

        Ok(Response::new(MoveCursorResponse {
            composing_text: Some(composing_text),
        }))
    }

    async fn clear_text(
        &self,
        _: Request<ClearTextRequest>,
    ) -> Result<Response<ClearTextResponse>, Status> {
        let _ffi_guard = self.lock_ffi().map_err(ffi_error_status)?;
        clear_text();
        Ok(Response::new(ClearTextResponse {}))
    }

    async fn shrink_text(
        &self,
        request: Request<ShrinkTextRequest>,
    ) -> Result<Response<ShrinkTextResponse>, Status> {
        let _ffi_guard = self.lock_ffi().map_err(ffi_error_status)?;
        let composing_text = shrink_text(request.into_inner().offset)
            .and_then(convert)
            .map_err(ffi_error_status)?;

        Ok(Response::new(ShrinkTextResponse {
            composing_text: Some(composing_text),
        }))
    }

    async fn commit_prefix_and_append(
        &self,
        request: Request<CommitPrefixAndAppendRequest>,
    ) -> Result<Response<CommitPrefixAndAppendResponse>, Status> {
        let _ffi_guard = self.lock_ffi().map_err(ffi_error_status)?;
        let request = request.into_inner();
        let composing_text = commit_prefix_and_append(request.offset, &request.text_to_append)
            .and_then(convert)
            .map_err(ffi_error_status)?;

        Ok(Response::new(CommitPrefixAndAppendResponse {
            composing_text: Some(composing_text),
        }))
    }

    async fn set_context(
        &self,
        request: Request<shared::proto::SetContextRequest>,
    ) -> Result<Response<shared::proto::SetContextResponse>, Status> {
        let _ffi_guard = self.lock_ffi().map_err(ffi_error_status)?;
        let context = request.into_inner().context;
        let trimmed_context = context
            .split('\r')
            .rfind(|line| !line.is_empty())
            .unwrap_or_default();
        let context = make_c_string(trimmed_context, "context").map_err(ffi_error_status)?;

        // SAFETY: context is NUL-terminated and valid for the duration of the call.
        let status = unsafe { SetContext(context.as_ptr()) };
        check_status("SetContext", status).map_err(ffi_error_status)?;
        Ok(Response::new(shared::proto::SetContextResponse {}))
    }

    async fn update_config(
        &self,
        _: Request<shared::proto::UpdateConfigRequest>,
    ) -> Result<Response<shared::proto::UpdateConfigResponse>, Status> {
        let _ffi_guard = self.lock_ffi().map_err(ffi_error_status)?;
        // SAFETY: LoadConfig takes no pointers and returns a fixed-width status code.
        let status = unsafe { LoadConfig() };
        check_status("LoadConfig", status).map_err(ffi_error_status)?;
        Ok(Response::new(shared::proto::UpdateConfigResponse {}))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("AzookeyServer started");
    let current_exe = std::env::current_exe()?;
    let parent_dir = current_exe.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "server executable has no parent directory",
        )
    })?;
    let parent_dir = parent_dir.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "server executable directory is not valid UTF-8",
        )
    })?;
    initialize(parent_dir)?;

    let service = MyAzookeyService::default();

    println!("AzookeyServer listening");

    Server::builder()
        .add_service(AzookeyServiceServer::new(service))
        .add_service(
            ReflectionBuilder::configure()
                .register_encoded_file_descriptor_set(shared::proto::FILE_DESCRIPTOR_SET)
                .build_v1()
                .unwrap(),
        )
        .serve_with_incoming(TonicNamedPipeServer::new("azookey_server"))
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn ffi_uses_fixed_width_counts() {
        let candidate = FFICandidate {
            text: ptr::null_mut(),
            subtext: ptr::null_mut(),
            corresponding_count: i32::MAX,
        };
        assert_eq!(std::mem::size_of::<i32>(), 4);
        assert_eq!(std::mem::size_of_val(&candidate.corresponding_count), 4);
    }

    #[test]
    fn null_empty_candidate_list_is_valid() {
        assert_eq!(candidate_list_count(ptr::null_mut(), 0).unwrap(), 0);
    }

    #[test]
    fn null_nonempty_candidate_list_is_rejected() {
        assert!(matches!(
            candidate_list_count(ptr::null_mut(), 1),
            Err(FfiError::CallFailed("GetComposedText"))
        ));
    }

    #[test]
    fn negative_candidate_count_is_rejected() {
        assert!(matches!(
            candidate_list_count(ptr::null_mut(), -1),
            Err(FfiError::InvalidCandidateCount(-1))
        ));
    }

    #[test]
    fn c_string_validation_accepts_empty_and_rejects_embedded_nul() {
        assert!(make_c_string("", "test").is_ok());
        assert!(matches!(
            make_c_string("a\0b", "test"),
            Err(FfiError::InteriorNul("test"))
        ));
    }

    #[test]
    fn composing_text_raw_input_is_independent_of_candidates() {
        let composing_text = ComposingText {
            hiragana: "きょ".to_owned(),
            suggestions: Vec::new(),
            raw_input: "kyo".to_owned(),
        };
        assert!(composing_text.suggestions.is_empty());
        assert_eq!(composing_text.raw_input, "kyo");
    }
}
