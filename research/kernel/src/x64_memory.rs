//! Streaming AMD64 virtual memory utilities; protocol/pattern semantics shared with ARM64.
use super::super::{holes_json, merge_hole, search_pattern, MAX_SEARCH_BYTES};
use super::*;

const MAX_DUMP_BYTES: u64 = 64 * 1024 * 1024;
// Count candidate visits and fixed-byte comparisons, independent of wall time.
// An adversarial pattern must not hold the synchronous command loop indefinitely.
const MAX_SEARCH_WORK: u64 = 64 * 1024 * 1024;

struct Work {
    remaining: u64,
}

impl Work {
    fn spend(&mut self) -> Result<(), String> {
        if self.remaining == 0 {
            return Err("memory.search work budget exhausted; reduce length or increase align and retry (no partial result returned)".into());
        }
        self.remaining -= 1;
        Ok(())
    }
}

fn range(q: &Value, maximum: u64) -> Result<(u64, u64), String> {
    let address = number(&q["address"])?;
    let length = number(&q["length"])?;
    if length == 0 || length > maximum || address.checked_add(length).is_none() {
        return Err(format!(
            "length: 1..{maximum} bytes without address overflow"
        ));
    }
    Ok((address, length))
}

struct Search {
    address: u64,
    length: u64,
    pattern: Vec<Option<u8>>,
    align: u64,
    limit: u64,
}

impl Search {
    fn parse(q: &Value) -> Result<Self, String> {
        let (address, length) = range(q, MAX_SEARCH_BYTES)?;
        let pattern = search_pattern(q)?;
        let optional = |key: &str, default| {
            q.get(key)
                .filter(|v| !v.is_null())
                .map(number)
                .transpose()
                .map(|n| n.unwrap_or(default))
        };
        let align = optional("align", 1)?;
        if !align.is_power_of_two() || align > 4096 {
            return Err("align: a power of two up to 4096".into());
        }
        let limit = optional("limit", 100)?;
        if !(1..=10_000).contains(&limit) {
            return Err("limit: 1..10000 hits".into());
        }
        Ok(Self {
            address,
            length,
            pattern,
            align,
            limit,
        })
    }

    fn run(&self, read: impl FnMut(u64, &mut [u8]) -> Result<(), String>) -> Result<Value, String> {
        self.run_with_work(
            read,
            &mut Work {
                remaining: MAX_SEARCH_WORK,
            },
        )
    }

    fn run_with_work(
        &self,
        mut read: impl FnMut(u64, &mut [u8]) -> Result<(), String>,
        work: &mut Work,
    ) -> Result<Value, String> {
        let mut fixed: Vec<_> = self
            .pattern
            .iter()
            .enumerate()
            .filter_map(|(i, byte)| byte.map(|b| (i, b)))
            .collect();
        let mut frequency = [0usize; 256];
        for &(_, byte) in &fixed {
            frequency[byte as usize] += 1;
        }
        // The least frequent fixed byte in the pattern is a useful anchor for
        // repeated-byte near-matches. Prefer the last occurrence on ties.
        let anchor = (0..fixed.len())
            .rev()
            .min_by_key(|&i| frequency[fixed[i].1 as usize])
            .expect("validated fixed byte");
        fixed.swap(0, anchor);
        let (mut hits, mut holes, mut scanned, mut next) = (Vec::new(), Vec::new(), 0u64, None);
        let mut carry = Vec::new();
        let end = self.address + self.length;
        let mut at = self.address;
        let mut page = [0; 4096];
        while at < end {
            // Subtraction, rather than rounding the page end up, also works
            // within the last page of the u64 address space.
            let n = (4096 - (at & 4095)).min(end - at) as usize;
            if read(at, &mut page[..n]).is_err() {
                merge_hole(&mut holes, at, n as u64);
                carry.clear();
                at += n as u64;
                continue;
            }
            scanned += n as u64;
            let base = at - carry.len() as u64;
            carry.extend_from_slice(&page[..n]);
            let candidates = (carry.len() + 1).saturating_sub(self.pattern.len());
            let first = (self.align - base % self.align) % self.align;
            for offset in (first as usize..candidates).step_by(self.align as usize) {
                work.spend()?;
                let mut matched = true;
                for &(index, byte) in &fixed {
                    work.spend()?;
                    if carry[offset + index] != byte {
                        matched = false;
                        break;
                    }
                }
                if !matched {
                    continue;
                }
                let hit = base + offset as u64;
                if hits.len() as u64 == self.limit {
                    next = Some(hit);
                    break;
                }
                hits.push(hit);
            }
            if next.is_some() {
                break;
            }
            let keep = carry.len().min(self.pattern.len() - 1);
            carry.drain(..carry.len() - keep);
            at += n as u64;
        }
        Ok(
            json!({"hits":hits,"pattern_length":self.pattern.len(),"scanned":scanned,
            "holes":holes_json(&holes),"truncated":next.is_some(),"next":next}),
        )
    }
}

/// Write only bounded page slices. A failed read may have partially modified
/// its output; clear it before zero-filling so no partial bytes leak into holes.
fn dump_to(
    address: u64,
    length: u64,
    skip: bool,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), String>,
    mut out: impl Write,
) -> Result<Vec<(u64, u64)>, String> {
    let end = address.checked_add(length).ok_or("range overflow")?;
    let mut at = address;
    let mut holes = Vec::new();
    let mut page = [0; 4096];
    while at < end {
        let n = (4096 - (at & 4095)).min(end - at) as usize;
        if let Err(e) = read(at, &mut page[..n]) {
            if !skip {
                return Err(e);
            }
            page[..n].fill(0);
            merge_hole(&mut holes, at, n as u64);
        }
        out.write_all(&page[..n]).map_err(string)?;
        at += n as u64;
    }
    Ok(holes)
}

fn dump_file(
    path: &Path,
    address: u64,
    length: u64,
    skip: bool,
    read: impl FnMut(u64, &mut [u8]) -> Result<(), String>,
) -> Result<Vec<(u64, u64)>, String> {
    // If create_new fails, never remove or overwrite the pre-existing file.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(string)?;
    let result = dump_to(address, length, skip, read, &mut file);
    drop(file);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

impl Session {
    pub(super) fn memory_search(&self, cpu: usize, q: &Value) -> Result<Value, String> {
        let search = Search::parse(q)?;
        let r = self.registers(cpu, q["context"].as_bool().unwrap_or(true))?;
        let space = AddressSpace {
            memory: &self.vm,
            registers: &r,
        };
        search.run(|at, data| space.read(at, data).map_err(string))
    }

    pub(super) fn memory_dump(&self, cpu: usize, q: &Value) -> Result<Value, String> {
        let (address, length) = range(q, MAX_DUMP_BYTES)?;
        // Like ARM64 dump, always use the selected process context.
        let r = self.registers(cpu, true)?;
        let space = AddressSpace {
            memory: &self.vm,
            registers: &r,
        };
        let exports = self.state.join("exports");
        std::fs::create_dir_all(&exports).map_err(string)?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(string)?
            .as_nanos();
        let path = exports.join(format!("memory-{address:016x}-{stamp}.bin"));
        let skip = q["skip_unmapped"].as_bool().unwrap_or(false);
        let holes = dump_file(&path, address, length, skip, |at, data| {
            space.read(at, data).map_err(string)
        })?;
        let mut reply = json!({"path":path,"address":address,"length":length,"cpu":cpu,
            "pid":self.selected.as_ref().map(|p| p.pid),"format":"raw virtual memory"});
        if skip {
            reply["format"] = json!("raw virtual memory, holes zero-filled");
            reply["unmapped"] = json!(holes.iter().map(|h| h.1).sum::<u64>());
            reply["holes"] = holes_json(&holes);
        }
        Ok(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search(q: Value, data: &[u8]) -> Value {
        Search::parse(&q)
            .unwrap()
            .run(|at, out| {
                out.copy_from_slice(&data[at as usize..at as usize + out.len()]);
                Ok(())
            })
            .unwrap()
    }

    #[test]
    fn patterns_and_validation() {
        for q in [
            json!({"hex":""}),
            json!({"hex":"??"}),
            json!({"hex":"a?"}),
            json!({"hex":"é"}),
            json!({"hex":"abc"}),
            json!({"text":""}),
            json!({"hex":"ab","text":"x"}),
            json!({"int":256,"width":1}),
            json!({"int":-129,"width":1}),
            json!({"int":1,"width":3}),
            json!({"text":"a".repeat(4097)}),
        ] {
            assert!(search_pattern(&q).is_err(), "{q}");
        }
        assert_eq!(
            search_pattern(&json!({"int":-128,"width":1})).unwrap(),
            vec![Some(128)]
        );
        assert_eq!(
            search_pattern(&json!({"text":"😀","utf16":true})).unwrap(),
            vec![Some(0x3d), Some(0xd8), Some(0), Some(0xde)]
        );
        let base = json!({"address":0,"length":1,"hex":"aa"});
        for (key, value) in [
            ("length", json!(0)),
            ("length", json!(MAX_SEARCH_BYTES + 1)),
            ("address", json!(u64::MAX)),
            ("align", json!(0)),
            ("align", json!(3)),
            ("align", json!(8192)),
            ("limit", json!(0)),
            ("limit", json!(10001)),
        ] {
            let mut q = base.clone();
            q[key] = value;
            assert!(Search::parse(&q).is_err(), "{q}");
        }
        assert!(range(
            &json!({"address":0,"length":MAX_DUMP_BYTES+1}),
            MAX_DUMP_BYTES
        )
        .is_err());
        assert!(range(&json!({"address":u64::MAX,"length":1}), MAX_DUMP_BYTES).is_err());
    }

    #[test]
    fn cross_page_wildcards_overlap_and_pagination() {
        let mut data = vec![0; 8192];
        data[4094..4099].copy_from_slice(b"ababa");
        let q = json!({"address":4093,"length":8,"hex":"61 ?? 61"});
        assert_eq!(search(q.clone(), &data)["hits"], json!([4094, 4096]));
        let mut limited = q.clone();
        limited["limit"] = json!(1);
        let result = search(limited, &data);
        assert_eq!(result["hits"], json!([4094]));
        assert_eq!(result["next"], json!(4096));
        assert_eq!(result["truncated"], true);
        let mut resume = q.clone();
        resume["address"] = result["next"].clone();
        resume["length"] = json!(5);
        assert_eq!(search(resume, &data)["hits"], json!([4096]));
        let mut aligned = q;
        aligned["align"] = json!(4096);
        assert_eq!(search(aligned, &data)["hits"], json!([4096]));
        assert_eq!(
            search(
                json!({"address":4094,"length":3,"text":"aba","limit":1}),
                &data
            )["truncated"],
            false
        );
    }

    #[test]
    fn integer_utf8_utf16_and_short_ranges() {
        assert_eq!(
            search(
                json!({"address":0,"length":4,"int":-1,"width":2}),
                &[0xff; 4]
            )["hits"],
            json!([0, 1, 2])
        );
        assert_eq!(
            search(json!({"address":0,"length":4,"text":"é"}), "éé".as_bytes())["hits"],
            json!([0, 2])
        );
        assert_eq!(
            search(
                json!({"address":0,"length":4,"text":"Ab","utf16":true}),
                b"A\0b\0"
            )["hits"],
            json!([0])
        );
        assert_eq!(
            search(json!({"address":0,"length":1,"hex":"aa bb"}), &[0xaa])["hits"],
            json!([])
        );
        let s = Search::parse(&json!({"address":7,"length":8192,"hex":"aa"})).unwrap();
        let result = s.run(|_, _| Err("missing".into())).unwrap();
        assert_eq!(result["scanned"], 0);
        assert_eq!(result["holes"], json!([{"address":7,"length":8192}]));
        assert_eq!(result["next"], Value::Null);
    }

    #[test]
    fn holes_merge_and_break_carry() {
        let s = Search::parse(&json!({"address":4094,"length":8196,"hex":"aa aa aa"})).unwrap();
        let result = s
            .run(|at, out| {
                out.fill(0xaa);
                if (4096..12288).contains(&at) {
                    Err("hole".into())
                } else {
                    Ok(())
                }
            })
            .unwrap();
        assert_eq!(result["hits"], json!([]));
        assert_eq!(result["holes"], json!([{"address":4096,"length":8192}]));
        assert_eq!(result["scanned"], 4);
    }

    #[test]
    fn maximum_pattern_and_last_address_page() {
        let data = vec![b'a'; 8192];
        let result = search(
            json!({"address":1,"length":8190,"text":"a".repeat(4096),"limit":10000}),
            &data,
        );
        assert_eq!(result["hits"].as_array().unwrap().len(), 4095);
        let s = Search::parse(&json!({"address":u64::MAX-2,"length":2,"hex":"ab"})).unwrap();
        assert_eq!(
            s.run(|_, out| {
                out.fill(0xab);
                Ok(())
            })
            .unwrap()["hits"],
            json!([u64::MAX - 2, u64::MAX - 1])
        );
    }

    #[test]
    fn repeated_byte_maximum_pattern_has_bounded_work() {
        let s = Search::parse(&json!({"address":0,"length":8192,
            "text":format!("{}b", "a".repeat(4095))}))
        .unwrap();
        // Exactly 4097 possible starts, each one visit + one failed anchor.
        // A naive left-to-right comparison would spend ~16 million comparisons.
        let mut work = Work {
            remaining: 4097 * 2,
        };
        let mut reads = 0;
        let result = s
            .run_with_work(
                |_, out| {
                    reads += 1;
                    out.fill(b'a');
                    Ok(())
                },
                &mut work,
            )
            .unwrap();
        assert_eq!(work.remaining, 0);
        assert_eq!(reads, 2);
        assert_eq!(result["hits"], json!([]));

        // Repeated bytes can also defeat an anchor. The budget, not timing,
        // guarantees that a long near-match cannot monopolize the lab loop.
        let s = Search::parse(&json!({"address":0,"length":MAX_SEARCH_BYTES,
            "text":"a".repeat(4096)}))
        .unwrap();
        let mut work = Work { remaining: 10_000 };
        let mut reads = 0;
        let error = s
            .run_with_work(
                |_, out| {
                    reads += 1;
                    out.fill(b'a');
                    out[4095] = b'b';
                    Ok(())
                },
                &mut work,
            )
            .unwrap_err();
        assert_eq!(work.remaining, 0);
        assert_eq!(reads, 2);
        assert!(error.contains("work budget exhausted"));
        assert!(error.contains("no partial result"));
    }

    #[test]
    fn alignment_is_filtered_before_comparisons() {
        let s = Search::parse(&json!({"address":0,"length":8192,
            "text":"a".repeat(4096),"align":4096}))
        .unwrap();
        // Only starts 0 and 4096: each costs a visit and 4096 comparisons.
        let mut work = Work {
            remaining: 2 * 4097,
        };
        let result = s
            .run_with_work(
                |_, out| {
                    out.fill(b'a');
                    Ok(())
                },
                &mut work,
            )
            .unwrap();
        assert_eq!(result["hits"], json!([0, 4096]));
        assert_eq!(work.remaining, 0);
    }

    #[test]
    fn failed_read_output_never_matches_or_becomes_carry() {
        let s = Search::parse(&json!({"address":4094,"length":4100,"hex":"aa bb"})).unwrap();
        let result = s
            .run(|at, out| {
                out.fill(0);
                match at {
                    4094 => {
                        out[1] = 0xaa;
                        Ok(())
                    }
                    4096 => {
                        // Would match previous carry, within the failed page, and
                        // across into the following page if failed bytes survived.
                        out[0] = 0xbb;
                        out[2..4].copy_from_slice(&[0xaa, 0xbb]);
                        out[4095] = 0xaa;
                        Err("partially modified before failure".into())
                    }
                    8192 => {
                        out[0] = 0xbb;
                        Ok(())
                    }
                    _ => panic!("unexpected read"),
                }
            })
            .unwrap();
        assert_eq!(result["hits"], json!([]));
        assert_eq!(result["scanned"], 4);
        assert_eq!(result["holes"], json!([{"address":4096,"length":4096}]));
    }

    #[test]
    fn dump_file_create_new_and_partial_failure_cleanup() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp = if Path::new("/tmp/opencode").is_dir() {
            PathBuf::from("/tmp/opencode")
        } else {
            std::env::temp_dir()
        };
        let path = temp.join(format!(
            "memory-dump-test-{}-{stamp}.bin",
            std::process::id()
        ));
        dump_file(&path, 0, 1, false, |_, out| {
            out.fill(7);
            Ok(())
        })
        .unwrap();
        assert!(dump_file(&path, 0, 1, false, |_, _| panic!(
            "must not read on create_new failure"
        ))
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), vec![7]);
        std::fs::remove_file(&path).unwrap();
        let mut reads = 0;
        assert!(dump_file(&path, 0, 8192, false, |_, out| {
            reads += 1;
            out.fill(9);
            if reads == 1 {
                Ok(())
            } else {
                Err("unmapped second page".into())
            }
        })
        .is_err());
        assert_eq!(reads, 2);
        assert!(!path.exists());
    }

    #[test]
    fn dump_zero_fills_partial_failed_reads_and_clips_pages() {
        let mut out = Vec::new();
        let holes = dump_to(
            4094,
            8196,
            true,
            |at, data| {
                data.fill(0xaa);
                if (4096..12288).contains(&at) {
                    Err("hole".into())
                } else {
                    Ok(())
                }
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(holes, vec![(4096, 8192)]);
        assert_eq!(out.len(), 8196);
        assert_eq!(&out[..2], &[0xaa; 2]);
        assert!(out[2..8194].iter().all(|&b| b == 0));
        assert_eq!(&out[8194..], &[0xaa; 2]);
        assert!(dump_to(0, 1, false, |_, _| Err("unmapped".into()), Vec::new()).is_err());
        assert!(dump_to(
            0,
            1,
            true,
            |_, out| {
                out.fill(1);
                Ok(())
            },
            std::io::Cursor::new(&mut [0u8; 0][..])
        )
        .is_err());
        let mut high = Vec::new();
        dump_to(
            u64::MAX - 1,
            1,
            false,
            |_, out| {
                out.fill(7);
                Ok(())
            },
            &mut high,
        )
        .unwrap();
        assert_eq!(high, vec![7]);
    }
}
