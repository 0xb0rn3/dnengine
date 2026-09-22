//! Just enough JSON to talk to the GUI.
//!
//! The CLI stays dependency free, and what it has to emit is small and fully known here: flat
//! objects of strings, numbers and booleans, plus arrays of those objects. Pulling in a
//! serializer for that would trade away the one property this tool is built around.

pub enum V { S(String), N(u64), B(bool), Null, Arr(Vec<Vec<(&'static str, V)>>), Strs(Vec<String>) }

pub fn s(v: impl Into<String>) -> V { V::S(v.into()) }

/// Escape per RFC 8259. Device models, mirror paths and filenames are outside our control, so
/// this has to be right rather than nearly right: one stray quote would corrupt the stream the
/// GUI is parsing.
fn esc(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    for c in input.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn val(v: &V) -> String {
    match v {
        V::S(x) => format!("\"{}\"", esc(x)),
        V::N(x) => x.to_string(),
        V::B(x) => x.to_string(),
        V::Null => "null".into(),
        V::Arr(items) => format!("[{}]", items.iter().map(|o| obj(o)).collect::<Vec<_>>().join(",")),
        V::Strs(items) => format!("[{}]",
            items.iter().map(|x| format!("\"{}\"", esc(x))).collect::<Vec<_>>().join(",")),
    }
}

pub fn obj(fields: &[(&'static str, V)]) -> String {
    let parts: Vec<String> = fields.iter().map(|(k, v)| format!("\"{}\":{}", esc(k), val(v))).collect();
    format!("{{{}}}", parts.join(","))
}

/// One object per line, flushed, so a GUI reading the pipe sees progress as it happens rather
/// than when the kernel decides the buffer is full.
pub fn line(fields: &[(&'static str, V)]) {
    use std::io::Write;
    println!("{}", obj(fields));
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hostile_device_model_cannot_break_the_stream() {
        let nasty = format!("San\"Disk\\{}{}", '\n', '\u{1}');
        let out = obj(&[("model", s(nasty)), ("size", V::N(7)), ("ok", V::B(true))]);
        assert_eq!(out, "{\"model\":\"San\\\"Disk\\\\\\n\\u0001\",\"size\":7,\"ok\":true}");
    }

    #[test]
    fn arrays_of_objects_round_trip_the_shape_the_gui_expects() {
        let out = obj(&[("devices", V::Arr(vec![vec![("name", s("sdc"))], vec![("name", s("sdd"))]]))]);
        assert_eq!(out, "{\"devices\":[{\"name\":\"sdc\"},{\"name\":\"sdd\"}]}");
    }
}
