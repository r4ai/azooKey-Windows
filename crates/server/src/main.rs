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

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, CStr, CString};
use std::fmt;
use tokio::sync::{Mutex, MutexGuard};

const USE_ZENZAI: bool = true;
const FFI_SUCCESS: i32 = 0;
const DEFAULT_SERVER_PIPE_NAME: &str = "azookey_server";
const SMOKE_TEST_PIPE_NAME_ENV: &str = "AZOOKEY_SERVER_SMOKE_TEST_PIPE_NAME";
const SMOKE_TEST_PIPE_NAME_PREFIX: &str = "azookey_server_smoke_";

fn is_valid_smoke_test_pipe_name(name: &str) -> bool {
    let Some(identifier) = name.strip_prefix(SMOKE_TEST_PIPE_NAME_PREFIX) else {
        return false;
    };
    identifier.len() == 32 && identifier.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn server_pipe_name() -> std::io::Result<String> {
    match std::env::var(SMOKE_TEST_PIPE_NAME_ENV) {
        Ok(name) if is_valid_smoke_test_pipe_name(&name) => Ok(name),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "{SMOKE_TEST_PIPE_NAME_ENV} must be '{SMOKE_TEST_PIPE_NAME_PREFIX}' followed by 32 hexadecimal characters"
            ),
        )),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_SERVER_PIPE_NAME.to_owned()),
        Err(std::env::VarError::NotUnicode(_)) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{SMOKE_TEST_PIPE_NAME_ENV} is not valid Unicode"),
        )),
    }
}

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
    fn RemoveText(count: i32, cursor: *mut i32) -> *mut c_char;
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

fn normalized_remove_count(count: u32) -> i32 {
    count.max(1).min(i32::MAX as u32) as i32
}

fn remove_text(count: i32) -> Result<RawComposingText, FfiError> {
    let mut cursor = 0;
    // SAFETY: count is positive and cursor is a valid output pointer.
    let result = unsafe { RemoveText(count, &mut cursor) };
    Ok(RawComposingText {
        text: copy_owned_string(result, "RemoveText")?,
    })
}

fn clear_text() {
    // SAFETY: ClearText takes no arguments and returns no owned data.
    unsafe { ClearText() };
}

fn normalize_context(context: &str) -> Result<String, FfiError> {
    let trimmed_context = context
        .split('\r')
        .rfind(|line| !line.is_empty())
        .unwrap_or_default();
    // Validate before a deferred SetContext is acknowledged. Otherwise an invalid context would
    // fail only after the client has already moved on to AppendText.
    make_c_string(trimmed_context, "context")?;
    Ok(trimmed_context.to_owned())
}

fn apply_context(context: &str) -> Result<(), FfiError> {
    let context = make_c_string(context, "context")?;
    // SAFETY: context is NUL-terminated and valid for the duration of the call.
    let status = unsafe { SetContext(context.as_ptr()) };
    check_status("SetContext", status)
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum IpcSessionToken {
    Legacy,
    Explicit(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingContext {
    observed_owner_generation: u64,
    context: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum OwnerAccess {
    Owned,
    ClaimedLegacy,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SetContextAccess {
    Apply,
    ClaimedLegacy,
    Staged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AppendAccess {
    Owned,
    ClaimedLegacy,
    ClaimedExplicit { context: String },
    Rejected,
}

#[derive(Debug, Default)]
struct FfiSessionState {
    active_session_token: Option<IpcSessionToken>,
    owner_generation: u64,
    pending_contexts: HashMap<IpcSessionToken, PendingContext>,
}

impl FfiSessionState {
    fn change_owner(&mut self, token: IpcSessionToken) {
        self.active_session_token = Some(token);
        self.owner_generation = self.owner_generation.wrapping_add(1);
        self.pending_contexts.clear();
    }

    /// Authorizes operations that may never start a new explicit session. Legacy clients retain
    /// their historical behavior and can claim on any stateful request.
    fn owner_access(&mut self, token: &IpcSessionToken) -> OwnerAccess {
        if self.active_session_token.as_ref() == Some(token) {
            return OwnerAccess::Owned;
        }

        if *token == IpcSessionToken::Legacy {
            self.change_owner(IpcSessionToken::Legacy);
            OwnerAccess::ClaimedLegacy
        } else {
            OwnerAccess::Rejected
        }
    }

    /// SetContext is the begin marker used by current clients, but a non-owner must not disturb
    /// the active converter yet. Its context is fenced to the observed owner generation and is
    /// applied only if the following AppendText wins that generation.
    fn set_context_access(&mut self, token: &IpcSessionToken, context: String) -> SetContextAccess {
        match self.owner_access(token) {
            OwnerAccess::Owned => SetContextAccess::Apply,
            OwnerAccess::ClaimedLegacy => SetContextAccess::ClaimedLegacy,
            OwnerAccess::Rejected => {
                self.pending_contexts.insert(
                    token.clone(),
                    PendingContext {
                        observed_owner_generation: self.owner_generation,
                        context,
                    },
                );
                SetContextAccess::Staged
            }
        }
    }

    /// AppendText is the only operation that can start a fresh explicit composition. The
    /// generation check prevents an append staged before a newer owner from reclaiming later.
    fn append_access(&mut self, token: &IpcSessionToken) -> AppendAccess {
        match self.owner_access(token) {
            OwnerAccess::Owned => AppendAccess::Owned,
            OwnerAccess::ClaimedLegacy => AppendAccess::ClaimedLegacy,
            OwnerAccess::Rejected => {
                let Some(pending) = self.pending_contexts.remove(token) else {
                    return AppendAccess::Rejected;
                };
                if pending.observed_owner_generation != self.owner_generation {
                    return AppendAccess::Rejected;
                }

                AppendAccess::ClaimedExplicit {
                    context: pending.context,
                }
            }
        }
    }
}

fn ipc_session_token<T>(request: &Request<T>) -> Result<IpcSessionToken, Status> {
    let Some(token) = request.metadata().get(shared::IPC_SESSION_METADATA_KEY) else {
        // Released DLLs predate per-session metadata. Treat all of their requests as one fixed
        // owner so they keep working while still participating in ownership transitions.
        return Ok(IpcSessionToken::Legacy);
    };
    let token = token.to_str().map_err(|_| {
        Status::invalid_argument(format!(
            "gRPC metadata '{}' must be ASCII",
            shared::IPC_SESSION_METADATA_KEY
        ))
    })?;
    if token.is_empty() {
        return Err(Status::invalid_argument(format!(
            "gRPC metadata '{}' must not be empty",
            shared::IPC_SESSION_METADATA_KEY
        )));
    }

    Ok(IpcSessionToken::Explicit(token.to_owned()))
}

#[derive(Debug, Default)]
pub struct MyAzookeyService {
    ffi_state: Mutex<FfiSessionState>,
}

impl MyAzookeyService {
    async fn lock_ffi(&self) -> MutexGuard<'_, FfiSessionState> {
        self.ffi_state.lock().await
    }
}

fn non_owner_status(operation: &'static str) -> Status {
    Status::failed_precondition(format!(
        "conversion session does not own the converter for {operation}"
    ))
}

#[tonic::async_trait]
impl AzookeyService for MyAzookeyService {
    async fn append_text(
        &self,
        request: Request<AppendTextRequest>,
    ) -> Result<Response<AppendTextResponse>, Status> {
        let token = ipc_session_token(&request)?;
        let input = request.into_inner().text_to_append;
        let mut ffi_state = self.lock_ffi().await;
        match ffi_state.append_access(&token) {
            AppendAccess::Owned => {}
            AppendAccess::ClaimedLegacy => clear_text(),
            AppendAccess::ClaimedExplicit { context } => {
                clear_text();
                apply_context(&context).map_err(ffi_error_status)?;
                // Commit logical ownership only after the deferred context has successfully
                // reached Swift. A SetContext failure leaves the previous owner authoritative.
                ffi_state.change_owner(token.clone());
            }
            AppendAccess::Rejected => return Err(non_owner_status("AppendText")),
        }
        let composing_text = add_text(&input)
            .and_then(convert)
            .map_err(ffi_error_status)?;

        Ok(Response::new(AppendTextResponse {
            composing_text: Some(composing_text),
        }))
    }

    async fn remove_text(
        &self,
        request: Request<RemoveTextRequest>,
    ) -> Result<Response<RemoveTextResponse>, Status> {
        let token = ipc_session_token(&request)?;
        let count = normalized_remove_count(request.get_ref().count);
        let mut ffi_state = self.lock_ffi().await;
        match ffi_state.owner_access(&token) {
            OwnerAccess::Owned => {}
            OwnerAccess::ClaimedLegacy => clear_text(),
            OwnerAccess::Rejected => return Err(non_owner_status("RemoveText")),
        }
        // ComposingText supports a counted surface deletion. Mutate once and regenerate
        // candidates once, regardless of how many auto-repeat events the client batched.
        let composing_text = remove_text(count)
            .and_then(convert)
            .map_err(ffi_error_status)?;

        Ok(Response::new(RemoveTextResponse {
            composing_text: Some(composing_text),
        }))
    }

    async fn move_cursor(
        &self,
        request: Request<MoveCursorRequest>,
    ) -> Result<Response<MoveCursorResponse>, Status> {
        let token = ipc_session_token(&request)?;
        let offset = request.into_inner().offset;
        let mut ffi_state = self.lock_ffi().await;
        match ffi_state.owner_access(&token) {
            OwnerAccess::Owned => {}
            OwnerAccess::ClaimedLegacy => clear_text(),
            OwnerAccess::Rejected => return Err(non_owner_status("MoveCursor")),
        }
        let composing_text = move_cursor(offset)
            .and_then(convert)
            .map_err(ffi_error_status)?;

        Ok(Response::new(MoveCursorResponse {
            composing_text: Some(composing_text),
        }))
    }

    async fn clear_text(
        &self,
        request: Request<ClearTextRequest>,
    ) -> Result<Response<ClearTextResponse>, Status> {
        let token = ipc_session_token(&request)?;
        let mut ffi_state = self.lock_ffi().await;
        match ffi_state.owner_access(&token) {
            OwnerAccess::Owned | OwnerAccess::ClaimedLegacy => clear_text(),
            // A late termination from an old explicit session must not clear the current owner.
            OwnerAccess::Rejected => {}
        }
        Ok(Response::new(ClearTextResponse {}))
    }

    async fn shrink_text(
        &self,
        request: Request<ShrinkTextRequest>,
    ) -> Result<Response<ShrinkTextResponse>, Status> {
        let token = ipc_session_token(&request)?;
        let offset = request.into_inner().offset;
        let mut ffi_state = self.lock_ffi().await;
        match ffi_state.owner_access(&token) {
            OwnerAccess::Owned => {}
            OwnerAccess::ClaimedLegacy => clear_text(),
            OwnerAccess::Rejected => return Err(non_owner_status("ShrinkText")),
        }
        let composing_text = shrink_text(offset)
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
        let token = ipc_session_token(&request)?;
        let message = request.into_inner();
        let mut ffi_state = self.lock_ffi().await;
        match ffi_state.owner_access(&token) {
            OwnerAccess::Owned => {}
            OwnerAccess::ClaimedLegacy => clear_text(),
            OwnerAccess::Rejected => return Err(non_owner_status("CommitPrefixAndAppend")),
        }
        let composing_text = commit_prefix_and_append(message.offset, &message.text_to_append)
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
        let token = ipc_session_token(&request)?;
        let context = normalize_context(&request.into_inner().context).map_err(ffi_error_status)?;
        let mut ffi_state = self.lock_ffi().await;
        match ffi_state.set_context_access(&token, context.clone()) {
            SetContextAccess::Apply => apply_context(&context).map_err(ffi_error_status)?,
            SetContextAccess::ClaimedLegacy => {
                clear_text();
                apply_context(&context).map_err(ffi_error_status)?;
            }
            SetContextAccess::Staged => {}
        }
        Ok(Response::new(shared::proto::SetContextResponse {}))
    }

    async fn update_config(
        &self,
        _: Request<shared::proto::UpdateConfigRequest>,
    ) -> Result<Response<shared::proto::UpdateConfigResponse>, Status> {
        let _ffi_guard = self.lock_ffi().await;
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
    // The override is deliberately restricted to GUID-suffixed smoke-test names so an
    // installed AzooKey can keep using the production pipe while a staged build is verified.
    let pipe_name = server_pipe_name()?;

    println!("AzookeyServer listening");

    Server::builder()
        .add_service(AzookeyServiceServer::new(service))
        .add_service(
            ReflectionBuilder::configure()
                .register_encoded_file_descriptor_set(shared::proto::FILE_DESCRIPTOR_SET)
                .build_v1()
                .unwrap(),
        )
        .serve_with_incoming(TonicNamedPipeServer::new(&pipe_name))
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

    #[test]
    fn remove_count_is_backward_compatible_and_bounded_for_ffi() {
        assert_eq!(normalized_remove_count(0), 1);
        assert_eq!(normalized_remove_count(1), 1);
        assert_eq!(normalized_remove_count(37), 37);
        assert_eq!(normalized_remove_count(u32::MAX), i32::MAX);
    }

    #[test]
    fn missing_session_metadata_uses_the_fixed_legacy_owner() {
        let request = Request::new(());

        assert_eq!(
            ipc_session_token(&request).unwrap(),
            IpcSessionToken::Legacy
        );
    }

    fn explicit_token(name: &str) -> IpcSessionToken {
        IpcSessionToken::Explicit(name.to_owned())
    }

    #[test]
    fn non_owner_context_is_staged_without_changing_the_owner() {
        let mut state = FfiSessionState::default();
        let token = explicit_token("tip-a");

        assert_eq!(
            state.set_context_access(&token, "context-a".to_owned()),
            SetContextAccess::Staged
        );
        assert_eq!(state.active_session_token, None);
        assert_eq!(state.owner_generation, 0);
        assert_eq!(
            state.pending_contexts.get(&token),
            Some(&PendingContext {
                observed_owner_generation: 0,
                context: "context-a".to_owned(),
            })
        );
    }

    #[test]
    fn staged_append_claims_and_carries_context_atomically() {
        let mut state = FfiSessionState::default();
        let token = explicit_token("tip-a");
        state.set_context_access(&token, "context-a".to_owned());

        assert_eq!(
            state.append_access(&token),
            AppendAccess::ClaimedExplicit {
                context: "context-a".to_owned(),
            }
        );
        assert_eq!(state.active_session_token, None);
        assert_eq!(state.owner_generation, 0);
        // The handler commits this only after apply_context succeeds.
        state.change_owner(token.clone());
        assert_eq!(state.active_session_token, Some(token));
        assert_eq!(state.owner_generation, 1);
        assert!(state.pending_contexts.is_empty());
    }

    #[test]
    fn newer_claim_fences_an_append_staged_for_the_previous_generation() {
        let mut state = FfiSessionState::default();
        let old = explicit_token("tip-a");
        let new = explicit_token("tip-b");
        state.set_context_access(&old, "context-a".to_owned());
        state.set_context_access(&new, "context-b".to_owned());

        assert!(matches!(
            state.append_access(&new),
            AppendAccess::ClaimedExplicit { .. }
        ));
        state.change_owner(new.clone());
        assert_eq!(state.append_access(&old), AppendAccess::Rejected);
        assert_eq!(state.active_session_token, Some(new));
        assert_eq!(state.owner_generation, 1);
    }

    #[test]
    fn delayed_context_after_a_claim_does_not_replace_or_clear_the_owner() {
        let mut state = FfiSessionState::default();
        let old = explicit_token("tip-a");
        let current = explicit_token("tip-b");
        state.set_context_access(&current, "context-b".to_owned());
        state.append_access(&current);
        state.change_owner(current.clone());

        assert_eq!(
            state.set_context_access(&old, "late-a".to_owned()),
            SetContextAccess::Staged
        );
        assert_eq!(state.active_session_token, Some(current));
        assert_eq!(state.owner_generation, 1);
    }

    #[test]
    fn non_owner_clear_is_a_noop_and_other_mutations_are_rejected() {
        let mut state = FfiSessionState::default();
        let owner = explicit_token("tip-a");
        let stale = explicit_token("tip-b");
        state.set_context_access(&owner, "context-a".to_owned());
        state.append_access(&owner);
        state.change_owner(owner.clone());
        let generation = state.owner_generation;

        // ClearText maps Rejected to a successful no-op; other mutation handlers map it to
        // failed_precondition. In both cases authorization leaves ownership untouched.
        assert_eq!(state.owner_access(&stale), OwnerAccess::Rejected);
        assert_eq!(state.owner_access(&stale), OwnerAccess::Rejected);
        assert_eq!(state.active_session_token, Some(owner));
        assert_eq!(state.owner_generation, generation);
    }

    #[test]
    fn owner_clear_does_not_advance_the_ownership_generation() {
        let mut state = FfiSessionState::default();
        let owner = explicit_token("tip-a");
        state.set_context_access(&owner, "context-a".to_owned());
        state.append_access(&owner);
        state.change_owner(owner.clone());
        let generation = state.owner_generation;

        assert_eq!(state.owner_access(&owner), OwnerAccess::Owned);
        assert_eq!(state.owner_generation, generation);
    }

    #[test]
    fn active_owner_context_applies_without_resetting_ownership() {
        let mut state = FfiSessionState::default();
        let owner = explicit_token("tip-a");
        state.set_context_access(&owner, "first".to_owned());
        state.append_access(&owner);
        state.change_owner(owner.clone());
        let generation = state.owner_generation;

        assert_eq!(
            state.set_context_access(&owner, "updated".to_owned()),
            SetContextAccess::Apply
        );
        assert_eq!(state.owner_generation, generation);
    }

    #[test]
    fn restarted_server_requires_context_before_the_first_explicit_append() {
        let mut state = FfiSessionState::default();
        let token = explicit_token("tip-a");

        assert_eq!(state.append_access(&token), AppendAccess::Rejected);
        assert_eq!(
            state.set_context_access(&token, "context-a".to_owned()),
            SetContextAccess::Staged
        );
        assert!(matches!(
            state.append_access(&token),
            AppendAccess::ClaimedExplicit { .. }
        ));
        state.change_owner(token.clone());
        assert_eq!(state.active_session_token, Some(token));
    }

    #[test]
    fn legacy_requests_keep_claim_on_any_stateful_operation_behavior() {
        let mut state = FfiSessionState::default();
        assert_eq!(
            state.owner_access(&IpcSessionToken::Legacy),
            OwnerAccess::ClaimedLegacy
        );
        assert_eq!(
            state.owner_access(&IpcSessionToken::Legacy),
            OwnerAccess::Owned
        );

        let explicit = explicit_token("tip-a");
        state.set_context_access(&explicit, "context-a".to_owned());
        state.append_access(&explicit);
        state.change_owner(explicit);
        assert_eq!(
            state.owner_access(&IpcSessionToken::Legacy),
            OwnerAccess::ClaimedLegacy
        );
        assert_eq!(state.active_session_token, Some(IpcSessionToken::Legacy));
    }

    #[test]
    fn smoke_test_pipe_name_requires_a_guid_suffix() {
        assert!(is_valid_smoke_test_pipe_name(
            "azookey_server_smoke_0123456789abcdef0123456789abcdef"
        ));
        assert!(!is_valid_smoke_test_pipe_name("azookey_server"));
        assert!(!is_valid_smoke_test_pipe_name(
            "azookey_server_smoke_not-a-guid"
        ));
    }

    #[test]
    fn swift_ffi_accepts_non_main_thread_calls() {
        let status = std::thread::spawn(|| {
            // SAFETY: LoadConfig takes no pointers and only reloads process-local state.
            unsafe { LoadConfig() }
        })
        .join()
        .expect("Swift FFI must not trap when called by a worker thread");

        assert_eq!(status, FFI_SUCCESS);
    }
}
