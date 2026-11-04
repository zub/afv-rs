//! The bearer token from the auth server.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let _ = afv_proto::api::token_expiry(data);
});
