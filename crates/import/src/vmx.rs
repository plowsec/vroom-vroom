//! The .vmx file: `key = "value"` lines. Keys are case-insensitive;
//! values escape special bytes as `|XX` (hex).

use std::collections::BTreeMap;

/// Biggest .vmx we read. Real ones are a few KiB.
pub const MAX_VMX: u64 = 1 << 20;

#[derive(Debug, Default, Clone)]
pub struct Vmx {
    map: BTreeMap<String, String>,
}

impl Vmx {
    pub fn parse(text: &str) -> Vmx {
        let mut map = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            let v = v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).unwrap_or(v);
            map.insert(k.trim().to_ascii_lowercase(), unescape(v));
        }
        Vmx { map }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(&key.to_ascii_lowercase()).map(String::as_str)
    }

    /// `TRUE`/`true`/`1` style flags; absent is `default`.
    pub fn flag(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            Some(v) => matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
            None => default,
        }
    }

    pub fn num(&self, key: &str) -> Option<u64> {
        self.get(key).and_then(|v| v.trim().parse().ok())
    }

    /// Keys starting with `prefix` (lowercase), e.g. every `ethernet0.*`.
    pub fn keys_with<'a>(&'a self, prefix: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.map.keys().map(String::as_str).filter(move |k| k.starts_with(prefix))
    }
}

fn unescape(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'|' {
            if let Some(x) = b.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(x);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fusion_lines() {
        let v = Vmx::parse(
            ".encoding = \"UTF-8\"\n# comment\nDisplayName = \"Win |22Dev|22 box\"\nnumvcpus = \"4\"\nnvme0:0.present = \"TRUE\"\nbroken line\nmemsize = 8192\nx = \"bad |zz escape|7C\"\n",
        );
        assert_eq!(v.get("displayname"), Some("Win \"Dev\" box"));
        assert_eq!(v.num("numvcpus"), Some(4));
        assert_eq!(v.num("memSize"), Some(8192));
        assert!(v.flag("NVME0:0.present", false));
        assert!(!v.flag("sound.present", false));
        assert_eq!(v.get("x"), Some("bad |zz escape|"));
        assert_eq!(v.keys_with("nvme0:").count(), 1);
    }
}
