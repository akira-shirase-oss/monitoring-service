//! The query-string decoders (serde_urlencoded, as axum's Query extractor uses):
//! the list/stats/incident filters. A query string must never panic the decode.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&which, query)) = data.split_first() else {
        return;
    };
    match which % 3 {
        0 => {
            let _ = serde_urlencoded::from_bytes::<monitor_server::ListParams>(query);
        }
        1 => {
            let _ = serde_urlencoded::from_bytes::<monitor_server::StatsParams>(query);
        }
        _ => {
            let _ = serde_urlencoded::from_bytes::<monitor_server::IncidentParams>(query);
        }
    }
});
