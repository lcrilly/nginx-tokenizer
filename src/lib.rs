// Copyright 2025 F5, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # nginx-tokenizer
//!
//! An NGINX dynamic module that counts LLM tokens in HTTP request and response bodies
//! as data streams through the proxy, without buffering.
//!
//! ## Directive
//!
//! ```nginx
//! llm_tokenize off | on | request | response [model=<name>];
//! ```
//!
//! - `off`      – tokenization disabled (default)
//! - `on`       – count tokens in both request and response bodies
//! - `request`  – count tokens in the request body only
//! - `response` – count tokens in the response body only
//! - `model=`   – optional OpenAI model name used to select the BPE encoding.
//!                Defaults to `o200k_base` (GPT-4o / o-series / GPT-5).
//!                Any model name accepted by `tiktoken_rs::get_bpe_from_model` is valid.
//!
//! ## Examples
//!
//! ```nginx
//! llm_tokenize on;                           # o200k_base (default)
//! llm_tokenize on model=gpt-4o;              # o200k_base  (explicit)
//! llm_tokenize on model=gpt-4;               # cl100k_base
//! llm_tokenize request model=gpt-3.5-turbo;  # cl100k_base, request body only
//! llm_tokenize response model=gpt-4o-mini;   # o200k_base,  response body only
//! ```
//!
//! ## Variables
//!
//! | Variable            | Description                                       |
//! |---------------------|---------------------------------------------------|
//! | `$llm_tokens`       | Token count for the upstream request body         |
//! | `$llm_tokens_sent`  | Token count for the downstream response body      |

use core::ffi::{c_char, c_void};
use core::ptr;
use std::sync::{Arc, OnceLock};
use std::collections::HashMap;

use ngx::core::Status;
use ngx::ffi::{
    NGX_CONF_TAKE1, NGX_CONF_TAKE2, NGX_HTTP_LOC_CONF, NGX_HTTP_LOC_CONF_OFFSET,
    NGX_HTTP_MODULE, NGX_HTTP_SRV_CONF, NGX_LOG_EMERG, NGX_OK, NGX_ERROR,
    ngx_buf_t, ngx_chain_t, ngx_command_t, ngx_conf_t, ngx_http_module_t,
    ngx_http_output_body_filter_pt, ngx_http_request_body_filter_pt,
    ngx_http_request_t, ngx_http_top_body_filter, ngx_http_top_request_body_filter,
    ngx_int_t, ngx_module_t, ngx_str_t, ngx_uint_t, ngx_variable_value_t,
};
use ngx::http::{self, HttpModule, HttpModuleLocationConf, MergeConfigError};
use ngx::{ngx_conf_log_error, ngx_log_debug_http, ngx_string};

use tiktoken_rs::{get_bpe_from_model, CoreBPE};

// ---------------------------------------------------------------------------
// Module-level filter chain pointers
// ---------------------------------------------------------------------------

/// Saved pointer to the next output (response) body filter in the chain.
static mut NGX_HTTP_NEXT_BODY_FILTER: ngx_http_output_body_filter_pt = None;

/// Saved pointer to the next request body filter in the chain.
static mut NGX_HTTP_NEXT_REQUEST_BODY_FILTER: ngx_http_request_body_filter_pt = None;

// ---------------------------------------------------------------------------
// BPE cache
// ---------------------------------------------------------------------------
//
// `get_bpe_from_model` is expensive (loads large BPE tables).  We keep a
// global cache keyed by model name so each unique model is initialised once
// per worker process.  The `Arc<CoreBPE>` is cloned cheaply into each
// `ModuleConfig` at parse time and shared across requests.

static BPE_CACHE: OnceLock<std::sync::Mutex<HashMap<String, Arc<CoreBPE>>>> = OnceLock::new();

fn bpe_cache() -> &'static std::sync::Mutex<HashMap<String, Arc<CoreBPE>>> {
    BPE_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Return an `Arc<CoreBPE>` for `model_name`, initialising and caching it on
/// first use.  Returns `Err` if the model name is not recognised by tiktoken-rs.
fn get_or_init_bpe(model_name: &str) -> Result<Arc<CoreBPE>, String> {
    let mut cache = bpe_cache().lock().expect("BPE cache mutex poisoned");
    if let Some(bpe) = cache.get(model_name) {
        return Ok(Arc::clone(bpe));
    }
    match get_bpe_from_model(model_name) {
        Ok(bpe) => {
            let arc = Arc::new(bpe);
            cache.insert(model_name.to_owned(), Arc::clone(&arc));
            Ok(arc)
        }
        Err(e) => Err(e.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Default model
// ---------------------------------------------------------------------------

/// Model name used when no `model=` argument is provided.
///
/// `o200k_base` covers GPT-4o, GPT-4o-mini, o1, o3, o4, GPT-5, GPT-4.1,
/// and their dated snapshot variants.
const DEFAULT_MODEL: &str = "gpt-4o";

// ---------------------------------------------------------------------------
// Module configuration
// ---------------------------------------------------------------------------

/// Which direction(s) to count tokens in.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum TokenizeMode {
    /// Disabled — no tokenization (default).
    #[default]
    Off,
    /// Count tokens in both request and response bodies.
    On,
    /// Count tokens in the request body only.
    Request,
    /// Count tokens in the response body only.
    Response,
}

/// Per-location configuration for the tokenizer module.
///
/// `bpe` is `None` at this level means "inherit from parent / use default".
/// It is resolved to a concrete `Arc<CoreBPE>` at parse time and stored here
/// so the hot path never touches the cache mutex.
#[derive(Default)]
pub struct ModuleConfig {
    /// Tokenization mode set by the `llm_tokenize` directive.
    pub mode: TokenizeMode,
    /// BPE tokenizer for this location, shared via `Arc`.
    /// `None` means not yet resolved; resolved during directive parsing.
    pub bpe: Option<Arc<CoreBPE>>,
}

impl http::Merge for ModuleConfig {
    fn merge(&mut self, prev: &ModuleConfig) -> Result<(), MergeConfigError> {
        // Inherit mode from the enclosing block when still at the default.
        if self.mode == TokenizeMode::Off && prev.mode != TokenizeMode::Off {
            self.mode = prev.mode;
        }
        // Inherit the tokenizer from the enclosing block when not set locally.
        if self.bpe.is_none() {
            self.bpe = prev.bpe.clone();
        }
        Ok(())
    }
}

impl ModuleConfig {
    /// Return the effective BPE tokenizer, falling back to the default model.
    ///
    /// The fallback path hits the cache mutex once; subsequent requests reuse
    /// the `Arc` stored in the config after the first call.  In practice the
    /// `None` branch is only reached for locations that set a mode without an
    /// explicit `model=`, and only for the very first request to that location.
    fn effective_bpe(&mut self) -> &Arc<CoreBPE> {
        if self.bpe.is_none() {
            // Lazily resolve the default.  Errors here are impossible because
            // DEFAULT_MODEL is a known-good constant.
            self.bpe = Some(
                get_or_init_bpe(DEFAULT_MODEL)
                    .expect("failed to initialise default BPE tokenizer"),
            );
        }
        self.bpe.as_ref().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Per-request context
// ---------------------------------------------------------------------------

/// Per-request mutable state stored in nginx's request context slot.
///
/// Allocated in the nginx request pool; freed automatically with the request.
struct RequestCtx {
    /// Accumulated token count for the **request** body.
    req_tokens: usize,
    /// Accumulated token count for the **response** body.
    resp_tokens: usize,
    /// Incomplete UTF-8 tail from the last request chunk (≤3 bytes).
    req_tail: [u8; 4],
    req_tail_len: usize,
    /// Incomplete UTF-8 tail from the last response chunk.
    resp_tail: [u8; 4],
    resp_tail_len: usize,
}

impl RequestCtx {
    fn new() -> Self {
        Self {
            req_tokens: 0,
            resp_tokens: 0,
            req_tail: [0u8; 4],
            req_tail_len: 0,
            resp_tail: [0u8; 4],
            resp_tail_len: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Streaming tokenizer helper
// ---------------------------------------------------------------------------

/// Feed `data` into the BPE tokenizer, handling split UTF-8 sequences at
/// buffer boundaries without accumulating the full body in memory.
///
/// `tail` / `tail_len` carry incomplete multi-byte sequences between calls.
/// Returns the number of new tokens found in this chunk.
fn count_tokens_streaming(
    bpe: &CoreBPE,
    data: &[u8],
    tail: &mut [u8; 4],
    tail_len: &mut usize,
) -> usize {
    if data.is_empty() {
        return 0;
    }

    // Prepend any bytes left over from the previous buffer.
    let combined: Vec<u8> = tail[..*tail_len].iter().chain(data.iter()).copied().collect();

    // Find the longest valid UTF-8 prefix.
    let (valid_str, remainder) = match std::str::from_utf8(&combined) {
        Ok(s) => (s, &combined[s.len()..]),
        Err(e) => {
            let up_to = e.valid_up_to();
            // SAFETY: valid_up_to is a guaranteed UTF-8 boundary.
            let s = unsafe { std::str::from_utf8_unchecked(&combined[..up_to]) };
            (s, &combined[up_to..])
        }
    };

    // Stash the trailing incomplete sequence (≤3 bytes) for the next call.
    let new_tail_len = remainder.len().min(tail.len());
    tail[..new_tail_len].copy_from_slice(&remainder[..new_tail_len]);
    *tail_len = new_tail_len;

    if valid_str.is_empty() {
        return 0;
    }

    // `encode_ordinary` skips special tokens — correct for arbitrary payloads.
    bpe.encode_ordinary(valid_str).len()
}

// ---------------------------------------------------------------------------
// Module struct and trait implementations
// ---------------------------------------------------------------------------

struct Module;

// ngx_str_t contains *mut u8 so rustc considers it !Sync by default.
// NGINX guarantees that these static name strings are read-only after
// preconfiguration, so it is safe to declare them Sync here.
struct SyncNgxStr(ngx_str_t);
unsafe impl Sync for SyncNgxStr {}

impl HttpModule for Module {
    fn module() -> &'static ngx_module_t {
        unsafe { &*ptr::addr_of!(ngx_http_tokenizer_module) }
    }

    /// Register `$llm_tokens` and `$llm_tokens_sent`.
    unsafe extern "C" fn preconfiguration(cf: *mut ngx_conf_t) -> ngx_int_t {
        static VAR_LLM_TOKENS:      SyncNgxStr = SyncNgxStr(ngx_string!("llm_tokens"));
        static VAR_LLM_TOKENS_SENT: SyncNgxStr = SyncNgxStr(ngx_string!("llm_tokens_sent"));

        let v = unsafe {
            ngx::ffi::ngx_http_add_variable(
                cf,
                &raw const VAR_LLM_TOKENS.0 as *mut ngx_str_t,
                ngx::ffi::NGX_HTTP_VAR_NOCACHEABLE as ngx_uint_t,
            )
        };
        if v.is_null() {
            return Status::NGX_ERROR.into();
        }
        unsafe {
            (*v).get_handler = Some(ngx_http_tokenizer_req_var_get);
            (*v).data = 0;
        }

        let v = unsafe {
            ngx::ffi::ngx_http_add_variable(
                cf,
                &raw const VAR_LLM_TOKENS_SENT.0 as *mut ngx_str_t,
                ngx::ffi::NGX_HTTP_VAR_NOCACHEABLE as ngx_uint_t,
            )
        };
        if v.is_null() {
            return Status::NGX_ERROR.into();
        }
        unsafe {
            (*v).get_handler = Some(ngx_http_tokenizer_resp_var_get);
            (*v).data = 0;
        }

        Status::NGX_OK.into()
    }

    /// Install both body filter hooks at the head of their respective chains.
    unsafe extern "C" fn postconfiguration(_cf: *mut ngx_conf_t) -> ngx_int_t {
        unsafe {
            NGX_HTTP_NEXT_BODY_FILTER = ngx_http_top_body_filter;
            ngx_http_top_body_filter = Some(ngx_http_tokenizer_body_filter);

            NGX_HTTP_NEXT_REQUEST_BODY_FILTER = ngx_http_top_request_body_filter;
            ngx_http_top_request_body_filter = Some(ngx_http_tokenizer_request_body_filter);
        }
        Status::NGX_OK.into()
    }
}

unsafe impl HttpModuleLocationConf for Module {
    type LocationConf = ModuleConfig;
}

// ---------------------------------------------------------------------------
// Module static data
// ---------------------------------------------------------------------------

// Accept 1 or 2 arguments:
//   llm_tokenize on;
//   llm_tokenize on model=gpt-4o;
const CONF_TAKE12: ngx_uint_t = (NGX_CONF_TAKE1 | NGX_CONF_TAKE2) as ngx_uint_t;

static mut NGX_HTTP_TOKENIZER_COMMANDS: [ngx_command_t; 2] = [
    ngx_command_t {
        name: ngx_string!("llm_tokenize"),
        type_: (NGX_HTTP_LOC_CONF | NGX_HTTP_SRV_CONF) as ngx_uint_t | CONF_TAKE12,
        set: Some(ngx_http_tokenizer_set_mode),
        conf: NGX_HTTP_LOC_CONF_OFFSET,
        offset: 0,
        post: ptr::null_mut(),
    },
    ngx_command_t::empty(),
];

static NGX_HTTP_TOKENIZER_MODULE_CTX: ngx_http_module_t = ngx_http_module_t {
    preconfiguration:  Some(Module::preconfiguration),
    postconfiguration: Some(Module::postconfiguration),
    create_main_conf:  None,
    init_main_conf:    None,
    create_srv_conf:   None,
    merge_srv_conf:    None,
    create_loc_conf:   Some(Module::create_loc_conf),
    merge_loc_conf:    Some(Module::merge_loc_conf),
};

#[cfg(feature = "export-modules")]
ngx::ngx_modules!(ngx_http_tokenizer_module);

#[used]
#[allow(non_upper_case_globals)]
#[cfg_attr(not(feature = "export-modules"), unsafe(no_mangle))]
pub static mut ngx_http_tokenizer_module: ngx_module_t = ngx_module_t {
    ctx:      &raw const NGX_HTTP_TOKENIZER_MODULE_CTX as _,
    commands: unsafe { &raw mut NGX_HTTP_TOKENIZER_COMMANDS[0] },
    type_:    NGX_HTTP_MODULE as _,
    ..ngx_module_t::default()
};

// ---------------------------------------------------------------------------
// Directive handler
// ---------------------------------------------------------------------------

/// Handler for `llm_tokenize (on|off|request|response) [model=<name>]`.
///
/// The `model=` argument is validated immediately against the tiktoken-rs
/// model table — an unknown model name is a hard configuration error so NGINX
/// will refuse to start rather than fail silently at request time.
extern "C" fn ngx_http_tokenizer_set_mode(
    cf: *mut ngx_conf_t,
    _cmd: *mut ngx_command_t,
    conf: *mut c_void,
) -> *mut c_char {
    unsafe {
        let conf = &mut *(conf as *mut ModuleConfig);
        let args: &[ngx_str_t] = (*(*cf).args).as_slice();

        // --- first argument: mode ---
        let mode_str = match args[1].to_str() {
            Ok(s) => s,
            Err(_) => {
                ngx_conf_log_error!(
                    NGX_LOG_EMERG, cf,
                    "`llm_tokenize` mode argument is not valid UTF-8"
                );
                return ngx::core::NGX_CONF_ERROR;
            }
        };

        conf.mode = match mode_str {
            v if v.eq_ignore_ascii_case("on")       => TokenizeMode::On,
            v if v.eq_ignore_ascii_case("off")      => TokenizeMode::Off,
            v if v.eq_ignore_ascii_case("request")  => TokenizeMode::Request,
            v if v.eq_ignore_ascii_case("response") => TokenizeMode::Response,
            _ => {
                ngx_conf_log_error!(
                    NGX_LOG_EMERG, cf,
                    "invalid value \"{}\" for `llm_tokenize`; \
                     expected on|off|request|response",
                    mode_str
                );
                return ngx::core::NGX_CONF_ERROR;
            }
        };

        // --- optional second argument: model=<name> ---
        let model_name: &str = if args.len() >= 3 {
            let opt_str = match args[2].to_str() {
                Ok(s) => s,
                Err(_) => {
                    ngx_conf_log_error!(
                        NGX_LOG_EMERG, cf,
                        "`llm_tokenize` model argument is not valid UTF-8"
                    );
                    return ngx::core::NGX_CONF_ERROR;
                }
            };
            match opt_str.strip_prefix("model=") {
                Some(name) if !name.is_empty() => name,
                _ => {
                    ngx_conf_log_error!(
                        NGX_LOG_EMERG, cf,
                        "invalid option \"{}\" for `llm_tokenize`; \
                         expected model=<name>",
                        opt_str
                    );
                    return ngx::core::NGX_CONF_ERROR;
                }
            }
        } else {
            DEFAULT_MODEL
        };

        // Resolve and validate the model name eagerly.  This also warms the
        // BPE cache so the first request to this location pays no extra cost.
        match get_or_init_bpe(model_name) {
            Ok(bpe) => conf.bpe = Some(bpe),
            Err(e) => {
                ngx_conf_log_error!(
                    NGX_LOG_EMERG, cf,
                    "unknown model \"{}\" for `llm_tokenize`: {}",
                    model_name, e
                );
                return ngx::core::NGX_CONF_ERROR;
            }
        }
    }

    ngx::core::NGX_CONF_OK
}

// ---------------------------------------------------------------------------
// Per-request context helpers
// ---------------------------------------------------------------------------

/// Retrieve the per-request `RequestCtx` for this module, creating it if absent.
///
/// # Safety
/// `r` must be a valid, non-null `ngx_http_request_t` pointer.
unsafe fn get_or_create_ctx(r: *mut ngx_http_request_t) -> *mut RequestCtx {
    unsafe {
        let module  = &*ptr::addr_of!(ngx_http_tokenizer_module);
        let ctx_ptr = *(*r).ctx.add(module.ctx_index);

        if !ctx_ptr.is_null() {
            return ctx_ptr as *mut RequestCtx;
        }

        let pool = ngx::core::Pool::from_ngx_pool((*r).pool);
        let ctx  = pool.allocate::<RequestCtx>(RequestCtx::new());
        *(*r).ctx.add(module.ctx_index) = ctx as *mut c_void;
        ctx
    }
}

// ---------------------------------------------------------------------------
// Response body filter
// ---------------------------------------------------------------------------

/// Output body filter — counts tokens in the response as it streams to the client.
extern "C" fn ngx_http_tokenizer_body_filter(
    r: *mut ngx_http_request_t,
    chain: *mut ngx_chain_t,
) -> ngx_int_t {
    unsafe {
        if r.is_null() {
            return NGX_ERROR as ngx_int_t;
        }

        let request = http::Request::from_ngx_http_request(r);

        // location_conf returns &ModuleConfig (immutable borrow of the config).
        // We need &mut to call effective_bpe() lazily, so we re-cast here.
        // This is safe because config mutation only happens during
        // configuration parsing (single-threaded) or the lazy BPE init below,
        // which is itself protected by the BPE cache mutex.
        let conf_ptr = match Module::location_conf(request) {
            Some(c) => c as *const ModuleConfig as *mut ModuleConfig,
            None => return NGX_HTTP_NEXT_BODY_FILTER.map_or(NGX_ERROR as _, |f| f(r, chain)),
        };
        let conf = &mut *conf_ptr;

        if !matches!(conf.mode, TokenizeMode::On | TokenizeMode::Response) {
            return NGX_HTTP_NEXT_BODY_FILTER.map_or(NGX_ERROR as _, |f| f(r, chain));
        }

        let bpe = Arc::clone(conf.effective_bpe());
        let ctx = get_or_create_ctx(r);

        let mut link = chain;
        while !link.is_null() {
            let buf: *mut ngx_buf_t = (*link).buf;
            if !buf.is_null() && !(*buf).pos.is_null() && !(*buf).last.is_null() {
                let len = (*buf).last.offset_from((*buf).pos) as usize;
                if len > 0 {
                    let data = std::slice::from_raw_parts((*buf).pos, len);
                    (*ctx).resp_tokens += count_tokens_streaming(
                        &bpe, data,
                        &mut (*ctx).resp_tail,
                        &mut (*ctx).resp_tail_len,
                    );
                }
            }
            link = (*link).next;
        }

        ngx_log_debug_http!(
            request,
            "llm_tokenize: response tokens so far = {}",
            (*ctx).resp_tokens
        );

        NGX_HTTP_NEXT_BODY_FILTER.map_or(NGX_ERROR as _, |f| f(r, chain))
    }
}

// ---------------------------------------------------------------------------
// Request body filter
// ---------------------------------------------------------------------------

/// Request body filter — counts tokens in the request as it streams from the client.
extern "C" fn ngx_http_tokenizer_request_body_filter(
    r: *mut ngx_http_request_t,
    chain: *mut ngx_chain_t,
) -> ngx_int_t {
    unsafe {
        if r.is_null() {
            return NGX_ERROR as ngx_int_t;
        }

        let request = http::Request::from_ngx_http_request(r);
        let conf_ptr = match Module::location_conf(request) {
            Some(c) => c as *const ModuleConfig as *mut ModuleConfig,
            None => {
                return NGX_HTTP_NEXT_REQUEST_BODY_FILTER
                    .map_or(NGX_ERROR as _, |f| f(r, chain));
            }
        };
        let conf = &mut *conf_ptr;

        if !matches!(conf.mode, TokenizeMode::On | TokenizeMode::Request) {
            return NGX_HTTP_NEXT_REQUEST_BODY_FILTER.map_or(NGX_ERROR as _, |f| f(r, chain));
        }

        let bpe = Arc::clone(conf.effective_bpe());
        let ctx = get_or_create_ctx(r);

        let mut link = chain;
        while !link.is_null() {
            let buf: *mut ngx_buf_t = (*link).buf;
            if !buf.is_null() && !(*buf).pos.is_null() && !(*buf).last.is_null() {
                let len = (*buf).last.offset_from((*buf).pos) as usize;
                if len > 0 {
                    let data = std::slice::from_raw_parts((*buf).pos, len);
                    (*ctx).req_tokens += count_tokens_streaming(
                        &bpe, data,
                        &mut (*ctx).req_tail,
                        &mut (*ctx).req_tail_len,
                    );
                }
            }
            link = (*link).next;
        }

        ngx_log_debug_http!(
            request,
            "llm_tokenize: request tokens so far = {}",
            (*ctx).req_tokens
        );

        NGX_HTTP_NEXT_REQUEST_BODY_FILTER.map_or(NGX_ERROR as _, |f| f(r, chain))
    }
}

// ---------------------------------------------------------------------------
// Variable get handlers
// ---------------------------------------------------------------------------

/// Get handler for `$llm_tokens` — request body token count.
unsafe extern "C" fn ngx_http_tokenizer_req_var_get(
    r: *mut ngx_http_request_t,
    v: *mut ngx_variable_value_t,
    _data: usize,
) -> ngx_int_t {
    unsafe {
        if r.is_null() || v.is_null() {
            return NGX_ERROR as ngx_int_t;
        }
        let module  = &*ptr::addr_of!(ngx_http_tokenizer_module);
        let ctx_ptr = *(*r).ctx.add(module.ctx_index);
        let count   = if ctx_ptr.is_null() {
            0
        } else {
            (*(ctx_ptr as *const RequestCtx)).req_tokens
        };
        set_variable_usize(r, v, count)
    }
}

/// Get handler for `$llm_tokens_sent` — response body token count.
unsafe extern "C" fn ngx_http_tokenizer_resp_var_get(
    r: *mut ngx_http_request_t,
    v: *mut ngx_variable_value_t,
    _data: usize,
) -> ngx_int_t {
    unsafe {
        if r.is_null() || v.is_null() {
            return NGX_ERROR as ngx_int_t;
        }
        let module  = &*ptr::addr_of!(ngx_http_tokenizer_module);
        let ctx_ptr = *(*r).ctx.add(module.ctx_index);
        let count   = if ctx_ptr.is_null() {
            0
        } else {
            (*(ctx_ptr as *const RequestCtx)).resp_tokens
        };
        set_variable_usize(r, v, count)
    }
}

/// Render `value` as a decimal string in the request pool and populate `v`.
///
/// # Safety
/// `r` and `v` must be valid, non-null pointers.
unsafe fn set_variable_usize(
    r: *mut ngx_http_request_t,
    v: *mut ngx_variable_value_t,
    value: usize,
) -> ngx_int_t {
    unsafe {
        let s     = value.to_string();
        let bytes = s.as_bytes();

        let pool = ngx::core::Pool::from_ngx_pool((*r).pool);
        let data = pool.alloc(bytes.len());
        if data.is_null() {
            (*v).set_not_found(1);
            (*v).set_valid(0);
            (*v).set_no_cacheable(0);
            return NGX_ERROR as ngx_int_t;
        }

        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data as *mut u8, bytes.len());

        (*v).set_len(bytes.len() as core::ffi::c_uint);
        (*v).set_valid(1);
        (*v).set_not_found(0);
        (*v).set_no_cacheable(1);
        (*v).data = data as *mut u8;

        NGX_OK as ngx_int_t
    }
}
