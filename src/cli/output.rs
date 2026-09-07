use serde::Serialize;
use std::io::Write as IoWrite;

/// Successful results go to stdout as pretty JSON; failures go to stderr as a JSON error
/// document with a stable `code`, so both halves of the CLI are scriptable.
pub(super) fn print_json<T: Serialize>(value: &T, stream: &mut dyn IoWrite, compact: bool) {
    let rendered = if compact {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    };
    match rendered {
        Ok(s) => {
            let _ = stream.write_all(s.as_bytes());
            let _ = stream.write_all(
                b"
",
            );
        }
        Err(err) => {
            let _ = writeln!(
                stream,
                "{{\"status\":\"error\",\"code\":\"json_error\",\"message\":{:?}}}",
                err.to_string()
            );
        }
    }
}
