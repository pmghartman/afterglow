//! afterglow --icons: app icons for a driver, one spec per line (or a JSON array) on
//! stdin, one JSON line per spec on stdout as each is done. Specs in docs/protocol.md

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as native;

#[cfg(not(windows))]
mod native {
    pub fn init() {}
    pub fn extract(_: &super::Spec) -> Option<Vec<u8>> {
        None
    }
}

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;

/// the icon size asked for, in pixels
pub const SIZE: u32 = 48;

/// what an icon is of
#[derive(Debug, PartialEq, Eq)]
pub enum Spec {
    /// f:<path> a file's shell icon, a cloud placeholder by attributes only
    File(String),
    /// a:<path> by file attributes only, the file never touched
    Attr(String),
    /// b: the recycle bin
    Bin,
    /// u:<AUMID> a Store app's tile logo
    Uwp(String),
    /// l:<path>,<index> an icon resource, index 0 when none is given
    Loc(String, i32),
}

/// None for a prefix or a body that names nothing
pub fn parse_spec(s: &str) -> Option<Spec> {
    let (k, v) = (s.get(..2)?, s.get(2..)?);
    match k {
        "b:" => Some(Spec::Bin),
        "f:" | "a:" | "u:" | "l:" if v.is_empty() => None,
        "f:" => Some(Spec::File(v.into())),
        "a:" => Some(Spec::Attr(v.into())),
        "u:" => Some(Spec::Uwp(v.into())),
        "l:" => Some(match v.rsplit_once(',').and_then(|(p, i)| Some((p, i.parse::<i32>().ok()?))) {
            Some((p, i)) if !p.is_empty() => Spec::Loc(p.into(), i),
            _ => Spec::Loc(v.into(), 0),
        }),
        _ => None,
    }
}

/// the specs of an input: a JSON array of strings, else one per non-blank line
pub fn parse_input(text: &str) -> Result<Vec<String>, String> {
    let t = text.trim_start_matches('\u{feff}').trim();
    if t.starts_with('[') {
        let v: serde_json::Value = serde_json::from_str(t).map_err(|e| format!("bad JSON: {e}"))?;
        let a = v.as_array().ok_or("not an array")?;
        return a.iter().map(|x| x.as_str().map(String::from).ok_or_else(|| "not a string".to_string())).collect();
    }
    Ok(t.lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.trim().is_empty()).map(String::from).collect())
}

/// {"k": spec, "png": base64 | null}
pub fn result_line(k: &str, png: Option<&[u8]>) -> String {
    serde_json::json!({ "k": k, "png": png.map(base64) }).to_string()
}

pub fn base64(d: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(d.len().div_ceil(3) * 4);
    for c in d.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        s.push(T[(n >> 18) as usize & 63] as char);
        s.push(T[(n >> 12) as usize & 63] as char);
        s.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        s.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    s
}

/// every spec answered, concurrently, a line as each finishes; a miss is null
pub fn serve(specs: &[String], out: &mut impl Write) {
    serve_with(specs, out, native::init, |s| parse_spec(s).and_then(|p| native::extract(&p)));
}

fn serve_with(specs: &[String], out: &mut impl Write, init: fn(), get: impl Fn(&str) -> Option<Vec<u8>> + Sync) {
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<String>();
    let workers = std::env::var("AFTERGLOW_ICON_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8));
    let workers = workers.clamp(1, 32).min(specs.len().max(1));
    std::thread::scope(|sc| {
        for _ in 0..workers {
            let tx = tx.clone();
            let (next, get) = (&next, &get);
            sc.spawn(move || {
                init();
                while let Some(k) = specs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let png = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| get(k))).ok().flatten();
                    if tx.send(result_line(k, png.as_deref())).is_err() {
                        return;
                    }
                }
            });
        }
        drop(tx);
        for l in rx {
            if writeln!(out, "{l}").and_then(|()| out.flush()).is_err() {
                break;
            }
        }
    });
}

/// stdin to stdout; the exit code
pub fn run() -> i32 {
    let mut text = String::new();
    if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut text) {
        eprintln!("afterglow: can't read stdin: {e}");
        return 1;
    }
    match parse_input(&text) {
        Ok(specs) => {
            serve(&specs, &mut std::io::stdout().lock());
            0
        }
        Err(e) => {
            eprintln!("afterglow: {e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parsed() {
        assert_eq!(parse_spec("f:C:\\a.exe"), Some(Spec::File("C:\\a.exe".into())));
        assert_eq!(parse_spec("a:x"), Some(Spec::Attr("x".into())));
        assert_eq!(parse_spec("b:"), Some(Spec::Bin));
        assert_eq!(parse_spec("u:Pkg_x!App"), Some(Spec::Uwp("Pkg_x!App".into())));
        assert_eq!(parse_spec("l:C:\\a.dll,-12"), Some(Spec::Loc("C:\\a.dll".into(), -12)));
        assert_eq!(parse_spec("l:C:\\a,b.dll"), Some(Spec::Loc("C:\\a,b.dll".into(), 0)));
        assert_eq!(parse_spec("l:C:\\a.ico"), Some(Spec::Loc("C:\\a.ico".into(), 0)));
    }

    #[test]
    fn bad_specs_name_nothing() {
        for s in ["", "f", "f:", "x:y", "u:", "l:", "é:x", "F:x"] {
            assert_eq!(parse_spec(s), None, "{s:?}");
        }
    }

    #[test]
    fn input_as_lines_or_json() {
        assert_eq!(parse_input("f:a\r\n\r\nb:\n  \nu:x!y").unwrap(), ["f:a", "b:", "u:x!y"]);
        assert_eq!(parse_input("[\"f:a\", \"b:\"]").unwrap(), ["f:a", "b:"]);
        assert_eq!(parse_input("").unwrap(), Vec::<String>::new());
        assert!(parse_input("[1]").is_err());
        assert!(parse_input("[\"a\"").is_err());
    }

    #[test]
    fn base64_as_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn a_miss_is_null_and_every_spec_answers() {
        let specs: Vec<String> = ["f:nope", "x:bad", "a:ok", "boom"].iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        serve_with(
            &specs,
            &mut out,
            || {},
            |k| match k {
                "a:ok" => Some(vec![1, 2, 3]),
                "boom" => panic!("a probe that fails"),
                _ => None,
            },
        );
        let mut got: Vec<serde_json::Value> =
            String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        got.sort_by_key(|v| v["k"].as_str().unwrap().to_string());
        let want = [("a:ok", Some("AQID")), ("boom", None), ("f:nope", None), ("x:bad", None)];
        assert_eq!(got.len(), want.len());
        for (g, (k, p)) in got.iter().zip(want) {
            assert_eq!(g["k"], k);
            assert_eq!(g["png"].as_str(), p);
            assert_eq!(g["png"].is_null(), p.is_none());
        }
    }

    #[test]
    fn unknown_specs_miss_through_the_real_path() {
        let mut out = Vec::new();
        serve(&["x:y".into(), "f:".into()], &mut out);
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s.lines().count(), 2);
        assert!(s.lines().all(|l| l.ends_with("\"png\":null}")));
    }
}
