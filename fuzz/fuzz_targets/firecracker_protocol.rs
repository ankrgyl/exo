#![no_main]

use exo_firecracker_protocol::{
    GuestProcessRequest, GuestRequest, GuestResponse, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    Message, decode_frame_length,
};
use libfuzzer_sys::fuzz_target;

type Request = Message<GuestRequest<GuestProcessRequest>>;
type Response = Message<GuestResponse>;

fn decode_payload(payload: &[u8]) {
    if payload.len() <= MAX_REQUEST_BYTES {
        drop(serde_json::from_slice::<Request>(payload));
    }
    if payload.len() <= MAX_RESPONSE_BYTES {
        drop(serde_json::from_slice::<Response>(payload));
    }
}

fuzz_target!(|data: &[u8]| {
    decode_payload(data);

    let Some(encoded_length) = data.get(..4).and_then(|bytes| bytes.try_into().ok()) else {
        return;
    };
    for maximum in [MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES] {
        let Ok(length) = decode_frame_length(encoded_length, maximum) else {
            continue;
        };
        let Some(payload) = data.get(4..).and_then(|bytes| bytes.get(..length)) else {
            continue;
        };
        decode_payload(payload);
    }
});
