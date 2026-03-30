#!/usr/bin/perl

# (C) nginx-tokenizer contributors
#
# Tests for the llm_tokenize module.

###############################################################################

use warnings;
use strict;

use Test::More;

BEGIN { use FindBin; chdir($FindBin::Bin); }

use lib 'lib';
use Test::Nginx;

###############################################################################

select STDERR; $| = 1;
select STDOUT; $| = 1;

my $t = Test::Nginx->new()->has(qw/http proxy/)->plan(8);

$t->write_file_expand('nginx.conf', <<'EOF');

%%TEST_GLOBALS%%

daemon off;

events {}

http {
    %%TEST_GLOBALS_HTTP%%

    # A tiny echo-like upstream served by a second server block.
    server {
        listen  127.0.0.1:8081;
        location / {
            return 200 "hello world this is a test response";
        }
    }

    server {
        listen  127.0.0.1:%%PORT_8080%%;
        server_name localhost;

        # Default: tokenization off
        location /off {
            llm_tokenize off;
            proxy_pass http://127.0.0.1:8081/;
        }

        # Count both directions
        location /on {
            llm_tokenize on;
            proxy_pass http://127.0.0.1:8081/;
            add_header X-Req-Tokens  $llm_tokens;
            add_header X-Resp-Tokens $llm_tokens_sent;
        }

        # Count request only
        location /request {
            llm_tokenize request;
            proxy_pass http://127.0.0.1:8081/;
            add_header X-Req-Tokens  $llm_tokens;
            add_header X-Resp-Tokens $llm_tokens_sent;
        }

        # Count response only
        location /response {
            llm_tokenize response;
            proxy_pass http://127.0.0.1:8081/;
            add_header X-Req-Tokens  $llm_tokens;
            add_header X-Resp-Tokens $llm_tokens_sent;
        }
    }
}
EOF

$t->run();

###############################################################################

# /off — variables should be 0 (module not active)
my $r = http_get('/off');
ok($r, 'off: got response');
like($r, qr/200 OK/, 'off: status 200');

# /on — both headers should be present and non-zero
$r = http_post_body('/on', 'The quick brown fox jumps over the lazy dog');
ok($r, 'on: got response');
my ($req_tok)  = $r =~ /X-Req-Tokens:\s*(\d+)/i;
my ($resp_tok) = $r =~ /X-Resp-Tokens:\s*(\d+)/i;
ok(defined $req_tok  && $req_tok  > 0, "on: req tokens = $req_tok");
ok(defined $resp_tok && $resp_tok > 0, "on: resp tokens = $resp_tok");

# /request — only req tokens should be non-zero
$r = http_post_body('/request', 'encode me please');
($req_tok)  = $r =~ /X-Req-Tokens:\s*(\d+)/i;
($resp_tok) = $r =~ /X-Resp-Tokens:\s*(\d+)/i;
ok(defined $req_tok && $req_tok > 0, "request: req tokens = $req_tok");
ok(!defined $resp_tok || $resp_tok == 0, 'request: resp tokens is 0');

# /response — only resp tokens should be non-zero
$r = http_get('/response');
($req_tok)  = $r =~ /X-Req-Tokens:\s*(\d+)/i;
($resp_tok) = $r =~ /X-Resp-Tokens:\s*(\d+)/i;
ok(defined $resp_tok && $resp_tok > 0, "response: resp tokens = $resp_tok");

###############################################################################

sub http_get {
    my ($url) = @_;
    return http(<<EOF);
GET $url HTTP/1.0\r
Host: localhost\r
\r
EOF
}

sub http_post_body {
    my ($url, $body) = @_;
    my $len = length($body);
    return http(<<EOF);
POST $url HTTP/1.0\r
Host: localhost\r
Content-Type: text/plain\r
Content-Length: $len\r
\r
$body
EOF
}
