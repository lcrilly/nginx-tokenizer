# nginx-tokenizer

An NGINX dynamic module, written in Rust using [ngx-rust], that counts
[OpenAI tiktoken] tokens in HTTP request and response bodies as they stream
through the proxy — without buffering.

Token counts are exposed as standard NGINX variables and can be used in
access logs, upstream selection, rate-limiting, or any other context that
accepts variables.

---

## Directive

**Syntax:**
```nginx
llm_tokenize off | on | request | response [model=<name>];
```

| Parameter          | Values       | Description                              |
|--------------------|--------------|------------------------------------------|
| `off` (default)    |              | Tokenization disabled                    |
| `on`               |              | Count tokens in both request and response bodies |
| `request`          |              | Count tokens in the request body only    |
| `response`         |              | Count tokens in the response body only   |
| `model` (optional) | OpenAI model | Selects the BPE encoding. Default: `o200k_base` (GPT-4o / o-series / GPT-5) |

**Context:** `http`, `server`, `location`

The directive is inherited from outer blocks and can be overridden at any
level, following standard NGINX merge semantics.

---

## Embedded Variables

`$llm_tokens`
 Token count for the proxied request body

`$llm_tokens_sent`
 Token count for the response body

Values are `0` when tokenization is disabled for that direction, or before
any body data has been seen for the current request.  Both variables are
**non-cacheable** so every evaluation reflects the latest running count.

---

## Supported Models and Encodings

The `model=` argument accepts any model name supported by [tiktoken-rs].
The name is validated at configuration load time — an unknown model is a
hard error that prevents NGINX from starting.

| Models (exact or prefix)                            | Encoding       |
|-----------------------------------------------------|----------------|
| `gpt-4o`, `gpt-4o-*`, `gpt-4.1`, `gpt-4.1-*`        | `o200k_base` ✦ |
| `gpt-5`, `gpt-5-*`                                  | `o200k_base` ✦ |
| `o1`, `o1-*`, `o3`, `o3-*`, `o4`, `o4-*`            | `o200k_base` ✦ |
| `chatgpt-4o-latest`, `chatgpt-4o-*`                 | `o200k_base` ✦ |
| `gpt-oss-*` (e.g. `gpt-oss-20b`, `gpt-oss-120b`)    | `o200k_harmony`|
| `gpt-4`, `gpt-4-*` (e.g. `gpt-4-0314`, `gpt-4-32k`) | `cl100k_base`  |
| `gpt-3.5-turbo`, `gpt-3.5-turbo-*`, `gpt-35-turbo-*`| `cl100k_base`  |
| `text-embedding-ada-002`, `text-embedding-3-*`      | `cl100k_base`  |
| `davinci-002`, `babbage-002`                        | `cl100k_base`  |
| `ft:gpt-4o*`, `ft:gpt-4*`, `ft:gpt-3.5-turbo*`      | `cl100k_base`  |
| `text-davinci-002`, `text-davinci-003`, `code-davinci-*`, `code-cushman-*` | `p50k_base` |
| `text-davinci-edit-001`, `code-davinci-edit-001` | `p50k_edit`    |
| `davinci`, `curie`, `babbage`, `ada`, `text-*-001`, legacy search/similarity | `r50k_base` |
| `gpt2`, `gpt-2`                                     | `r50k_base`    |

✦ **Default** — used when no `model=` is specified.

---

## Examples

```nginx
# Use default encoding (o200k_base) — suitable for GPT-4o and o-series.
llm_tokenize on;

# Explicit model names.
llm_tokenize on model=gpt-4o;
llm_tokenize on model=gpt-4o-mini;
llm_tokenize on model=gpt-4;
llm_tokenize on model=gpt-3.5-turbo;
llm_tokenize on model=o3-mini;

# Count only the request body, using gpt-4 tokenisation.
llm_tokenize request model=gpt-4;

# Count only the response body, using the default encoding.
llm_tokenize response;

# Disable tokenisation in a child location that inherits `on` from a parent.
llm_tokenize off;
```

---

## Build with Docker

The [Dockerfile](Dockerfile) in this repo will build a container based on the
`nginx:latest` Docker Official Image that includes the tokenizer module;
installed and loaded.

```bash
# Docker
docker build -t nginx:tokenizer .

# Apple Container
container build -t nginx:tokenizer
```

Now run a container from your new `nginx:tokenizer` image with a configuration that includes `llm_tokenize on;`.

## Build from source

### Requirements

| Dependency               | Min version |
|--------------------------|-------------|
| nginx sources            | 1.22.0      |
| Rust toolchain (`cargo`) | 1.81.0      |
| C compiler, PCRE2, zlib  | — |
| libclang (for `bindgen`) | — |

### Build steps

```bash
# Download the nginx sources for the current version
cd /tmp
NGINX_VER=`nginx -v 2>&1 | tr / - | cut -f3 -d' '`
wget -O - https://nginx.org/download/$NGINX_VER.tar.gz | tar xfz -

# Build the nginx-sys objects
cd $NGINX_VER
./configure --with-compat
make modules

# Return to the clone of this repo
NGINX_SOURCE_DIR=/tmp/$NGINX_VER cargo build --release
# Result: target/release/libnginx_tokenizer.so

# Copy and rename to default modules directory
cp target/release/libnginx_tokenizer.so /etc/nginx/modules/ngx_http_tokenizer_module.so
```

---

## Full Configuration Example

```nginx
# nginx.conf

load_module modules/ngx_http_tokenizer_module.so;

http {
    # Log both token counts for every proxied request.
    log_format llm '$remote_addr - $request '
                   'req_tokens=$llm_tokens '
                   'resp_tokens=$llm_tokens_sent';

    # Default to o200k_base for all API routes in this server block.
    server {
        listen 80;

        # GPT-4o endpoint — o200k_base (default, explicit for clarity)
        location /v1/gpt4o/ {
            llm_tokenize on model=gpt-4o;
            proxy_pass http://gpt4o_upstream;
            access_log /var/log/nginx/llm.log llm;
        }

        # GPT-4 endpoint — cl100k_base
        location /v1/gpt4/ {
            llm_tokenize on model=gpt-4;
            proxy_pass http://gpt4_upstream;
            access_log /var/log/nginx/llm.log llm;
        }

        # Embeddings — count only the request (what you're embedding)
        location /v1/embeddings/ {
            llm_tokenize request model=text-embedding-3-large;
            proxy_pass http://embeddings_upstream;
            access_log /var/log/nginx/llm.log llm;
        }

        # Legacy completions — count responses only
        location /v1/completions/ {
            llm_tokenize response model=gpt-3.5-turbo;
            proxy_pass http://completions_upstream;
            access_log /var/log/nginx/llm.log llm;
        }
    }
}
```

---

## Streaming Behaviour

NGINX delivers body data to filter modules in chunks (nginx chain buffers).
This module processes each chunk as it arrives:

1. For **response** bodies: hooks into `ngx_http_top_body_filter` and
   inspects every outbound buffer before forwarding to the next filter.
2. For **request** bodies: hooks into `ngx_http_top_request_body_filter`
   and inspects every inbound buffer as it is read from the client.

No data is copied or buffered beyond the ≤3 bytes needed to handle
multi-byte UTF-8 sequences that span buffer boundaries.

BPE tables for each encoding are initialised once (per worker process) on
first use and cached for the lifetime of the process.

---

## Building and Testing

```bash
# Lint
cargo fmt --check
cargo clippy -- -D warnings

# Unit tests
cargo test

# Integration tests (requires NGINX sources and nginx-tests)
export NGINX_SOURCE_DIR=../nginx
export NGINX_TESTS_DIR=../nginx-tests
make test
```

---

## Project Layout

```
nginx-tokenizer/
├── auto/
│   └── rust          # Build helper — sourced by the config script
├── src/
│   └── lib.rs        # Module implementation
├── t/                # Integration test files (*.t)
├── .editorconfig
├── .dockerignore
├── .gitignore
├── build.rs          # Cargo build script (passes nginx-sys features)
├── Cargo.toml
├── config            # NGINX build-system integration
├── config.make       # NGINX build-system Makefile fragments
├── Dockerfile        # Quickstart for container environments
├── LICENSE           # Apache 2.0
├── Makefile
├── README.md
└── rustfmt.toml
```

---

## License

Copyright 2025.  Licensed under the [Apache License, Version 2.0](LICENSE).

[ngx-rust]: https://github.com/nginx/ngx-rust
[tiktoken-rs]: https://github.com/zurawiki/tiktoken-rs
[OpenAI tiktoken]: https://github.com/openai/tiktoken
