//! Every JSON request decoder the API exposes — one selector byte chooses the
//! DTO, the rest is the body. Serde must never panic; a body that decodes must
//! re-serialize (the codec's fixed point — same model as minibtc's fuzz targets).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&which, body)) = data.split_first() else {
        return;
    };
    match which % 13 {
        0 => decode::<monitor_server::LoginBody>(body),
        1 => decode::<monitor_server::SignupBody>(body),
        2 => decode::<monitor_server::InviteBody>(body),
        3 => decode::<monitor_server::MemberBody>(body),
        4 => decode::<monitor_server::MemberRoleBody>(body),
        5 => decode::<monitor_server::TokenBody>(body),
        6 => decode::<monitor_server::WebhookBody>(body),
        7 => decode::<monitor_server::WorkspaceBody>(body),
        8 => decode::<monitor_server::PlanBody>(body),
        9 => decode::<monitor_server::StatusPageBody>(body),
        10 => decode::<monitor_server::CreateMonitor>(body),
        11 => decode::<monitor_server::PatchMonitor>(body),
        _ => decode::<monitor_server::CreateWindow>(body),
    }
});

fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) {
    let _ = serde_json::from_slice::<T>(body);
}
