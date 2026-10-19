//! Campaign-wide state: the corpus every instance shares, global coverage,
//! unique findings and each instance's counters. A local run feeds it
//! directly; a master feeds it from its workers (`sync`). The dashboard
//! (`tui`) and `stats.json` read it.

use crate::{BlockCoverage, CorpusStore, CoverageBlock, CoverageMap, Finding, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the epoch: instances on other machines report times too.
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// An instance that hasn't synchronized for this long is shown as lost.
pub const LOST_AFTER_SECS: f64 = 15.0;

/// Execs/s samples kept for the dashboard's history (one per second).
const HISTORY: usize = 600;

/// One corpus input and the coverage it reached. The snapshot is the same
/// everywhere, so a receiver takes the coverage instead of running it.
/// `blocks: None` asks the receiver to run it (a corpus resumed from disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub hash: String,
    #[serde(with = "hex_bytes")]
    pub data: Vec<u8>,
    #[serde(default)]
    pub blocks: Option<Vec<u64>>,
    #[serde(default)]
    pub edges: Vec<(u64, u64)>,
    /// Signatures of the findings this input produced.
    #[serde(default)]
    pub findings: Vec<String>,
}

/// A finding new to the instance that reports it, with its input.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingReport {
    pub signature: String,
    pub finding: Finding,
    #[serde(with = "hex_bytes")]
    pub data: Vec<u8>,
}

/// One fuzzing instance's counters, as it last reported them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceStats {
    pub name: String,
    pub started: f64,
    pub execs: u64,
    /// Over the last few seconds.
    pub execs_per_sec: f64,
    /// Inputs that produced any finding.
    pub crashes: u64,
    pub timeouts: u64,
    /// Snapshot restores from disk (timeouts, guest resets).
    pub restores: u64,
    pub corpus: usize,
    pub blocks: usize,
    pub edges: usize,
    pub last_new_cov: Option<f64>,
    /// Mean guest time per input and in-place reset time, recent inputs.
    pub exec_us: u64,
    pub reset_us: u64,
    pub foreign_threads: u64,
    pub done: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct FindingRow {
    pub signature: String,
    pub finding: Finding,
    /// Instances that reported it as new to them.
    pub reports: u64,
    pub first_seen: f64,
    pub instance: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstanceRow {
    pub stats: InstanceStats,
    pub last_seen: f64,
}

#[derive(Debug)]
struct Shared {
    entry: Entry,
    origin: String,
}

#[derive(Debug)]
pub struct Hub {
    pub title: String,
    pub started: f64,
    /// Block manifest: totals and function names for the dashboard.
    pub blocks: Vec<CoverageBlock>,
    covered: BTreeMap<u64, BlockCoverage>,
    edges: BTreeSet<(u64, u64)>,
    corpus: Vec<Shared>,
    hashes: HashSet<String>,
    findings: BTreeMap<String, FindingRow>,
    instances: BTreeMap<String, InstanceRow>,
    pub last_new_cov: Option<f64>,
    pub last_finding: Option<f64>,
    /// Total execs/s, one sample per `sample` call.
    pub history: VecDeque<u64>,
    last_sample: Option<(f64, u64)>,
    /// Inputs instances offered that added nothing campaign-wide.
    pub redundant: u64,
    store: Option<CorpusStore>,
    output: Option<PathBuf>,
    /// Ask every instance to finish (duration reached, `q` pressed).
    pub stop: bool,
    /// Distributed (a master) rather than one local instance.
    pub master: bool,
}

impl Hub {
    /// `store`: where accepted inputs and findings are saved (a master's
    /// output; a local run saves its own). `output`: where `stats.json` and
    /// `coverage.json` go.
    pub fn new(
        title: impl Into<String>,
        blocks: Vec<CoverageBlock>,
        store: Option<CorpusStore>,
        output: Option<PathBuf>,
    ) -> Self {
        Hub {
            title: title.into(),
            started: now(),
            blocks,
            covered: BTreeMap::new(),
            edges: BTreeSet::new(),
            corpus: Vec::new(),
            hashes: HashSet::new(),
            findings: BTreeMap::new(),
            instances: BTreeMap::new(),
            last_new_cov: None,
            last_finding: None,
            history: VecDeque::new(),
            last_sample: None,
            redundant: 0,
            store,
            output,
            stop: false,
            master: false,
        }
    }

    /// Inputs without known coverage (a resumed corpus): instances run them.
    pub fn add_unverified(&mut self, inputs: Vec<Vec<u8>>) {
        for data in inputs {
            let hash = CorpusStore::hash(&data);
            if self.hashes.insert(hash.clone()) {
                self.corpus.push(Shared {
                    entry: Entry {
                        hash,
                        data,
                        blocks: None,
                        edges: Vec::new(),
                        findings: Vec::new(),
                    },
                    origin: String::new(),
                });
            }
        }
    }

    /// Take an instance's report. Entries that add a block, an edge or a
    /// finding signature campaign-wide join the shared corpus.
    pub fn submit(
        &mut self,
        stats: InstanceStats,
        entries: Vec<Entry>,
        findings: Vec<FindingReport>,
    ) -> Result<()> {
        let at = now();
        let instance = stats.name.clone();
        for entry in entries {
            let Some(blocks) = &entry.blocks else {
                continue;
            };
            if self.hashes.contains(&entry.hash) {
                continue;
            }
            let new_blocks = blocks
                .iter()
                .filter(|id| !self.covered.contains_key(id))
                .count();
            let new_edges = entry
                .edges
                .iter()
                .filter(|edge| !self.edges.contains(edge))
                .count();
            let new_signature = entry
                .findings
                .iter()
                .any(|signature| !self.findings.contains_key(signature));
            if new_blocks == 0 && new_edges == 0 && !new_signature {
                self.redundant += 1;
                continue;
            }
            for id in blocks {
                let row = self.covered.entry(*id).or_default();
                row.hits += 1;
                row.first_input.get_or_insert_with(|| entry.hash.clone());
            }
            self.edges.extend(entry.edges.iter().copied());
            if new_blocks != 0 || new_edges != 0 {
                self.last_new_cov = Some(at);
            }
            if let Some(store) = &self.store {
                store.save_input(&entry.data, new_blocks)?;
            }
            self.hashes.insert(entry.hash.clone());
            self.corpus.push(Shared {
                entry,
                origin: instance.clone(),
            });
        }
        for report in findings {
            match self.findings.get_mut(&report.signature) {
                Some(row) => row.reports += 1,
                None => {
                    if let Some(store) = &self.store {
                        store.save_finding(&report.finding, &report.data)?;
                    }
                    self.last_finding = Some(at);
                    self.findings.insert(
                        report.signature.clone(),
                        FindingRow {
                            signature: report.signature,
                            finding: report.finding,
                            reports: 1,
                            first_seen: at,
                            instance: instance.clone(),
                        },
                    );
                }
            }
        }
        self.instances.insert(
            instance,
            InstanceRow {
                stats,
                last_seen: at,
            },
        );
        Ok(())
    }

    /// Up to `max` shared entries from `cursor` on, except `instance`'s own.
    /// Returns them and the cursor to ask from next time.
    pub fn entries_since(&self, cursor: usize, instance: &str, max: usize) -> (Vec<Entry>, usize) {
        let end = self.corpus.len().min(cursor.saturating_add(max));
        let entries = self
            .corpus
            .get(cursor..end)
            .unwrap_or(&[])
            .iter()
            .filter(|shared| shared.origin != instance)
            .map(|shared| shared.entry.clone())
            .collect();
        (entries, end.max(cursor.min(self.corpus.len())))
    }

    /// Record total execs/s since the previous call (call about once a second).
    pub fn sample(&mut self) {
        let at = now();
        let execs = self.execs();
        if let Some((then, before)) = self.last_sample {
            let span = (at - then).max(1e-3);
            self.history
                .push_back((execs.saturating_sub(before) as f64 / span).round() as u64);
            while self.history.len() > HISTORY {
                self.history.pop_front();
            }
        }
        self.last_sample = Some((at, execs));
    }

    pub fn execs(&self) -> u64 {
        self.instances.values().map(|row| row.stats.execs).sum()
    }

    pub fn execs_per_sec(&self) -> f64 {
        self.live().fold(0.0, |sum, row| sum + row.stats.execs_per_sec)
    }

    /// Instances still running and recently heard from.
    pub fn live(&self) -> impl Iterator<Item = &InstanceRow> {
        let at = now();
        self.instances
            .values()
            .filter(move |row| !row.stats.done && at - row.last_seen < LOST_AFTER_SECS)
    }

    pub fn instances(&self) -> impl Iterator<Item = &InstanceRow> {
        self.instances.values()
    }

    pub fn findings(&self) -> impl Iterator<Item = &FindingRow> {
        self.findings.values()
    }

    pub fn corpus_len(&self) -> usize {
        self.corpus.len()
    }

    /// The corpus the dashboard shows: the shared one for a master, the
    /// instance's own (seeds included) for a local run.
    pub fn corpus_size(&self) -> usize {
        if self.master {
            self.corpus.len()
        } else {
            self.instances
                .values()
                .map(|row| row.stats.corpus)
                .max()
                .unwrap_or(0)
        }
    }

    pub fn covered_blocks(&self) -> usize {
        self.covered.len()
    }

    pub fn is_covered(&self, id: u64) -> bool {
        self.covered.contains_key(&id)
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Every instance that ever reported has finished.
    pub fn all_done(&self) -> bool {
        self.instances.values().all(|row| row.stats.done)
    }

    pub fn coverage_map(&self) -> CoverageMap {
        CoverageMap {
            executions: self.execs(),
            new_edges: self.edges.len() as u64,
            blocks: self.covered.clone(),
            edges: self.edges.clone(),
        }
    }

    /// Write `stats.json` (and, for a master, `coverage.json` and
    /// `blocks.json`) into the output directory.
    pub fn persist(&self) -> Result<()> {
        let Some(output) = &self.output else {
            return Ok(());
        };
        let stats = serde_json::json!({
            "title": self.title,
            "started": self.started,
            "updated": now(),
            "execs": self.execs(),
            "execs_per_sec": self.execs_per_sec(),
            "corpus": self.corpus_size(),
            "blocks": self.covered.len(),
            "total_blocks": self.blocks.len(),
            "edges": self.edges.len(),
            "redundant": self.redundant,
            "last_new_cov": self.last_new_cov,
            "last_finding": self.last_finding,
            "findings": self.findings.values().collect::<Vec<_>>(),
            "instances": self.instances.values().collect::<Vec<_>>(),
        });
        write_atomic(output.join("stats.json"), &serde_json::to_vec_pretty(&stats)?)?;
        if self.store.is_some() {
            write_atomic(
                output.join("coverage.json"),
                &serde_json::to_vec_pretty(&self.coverage_map())?,
            )?;
            write_atomic(output.join("blocks.json"), &serde_json::to_vec_pretty(&self.blocks)?)?;
        }
        Ok(())
    }
}

/// Readers (a dashboard on another terminal, a script) never see half a file.
fn write_atomic(path: PathBuf, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, &path)?;
    Ok(())
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let text = String::deserialize(deserializer)?;
        hex::decode(text).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BugClass;

    fn entry(data: &[u8], blocks: &[u64], edges: &[(u64, u64)]) -> Entry {
        Entry {
            hash: CorpusStore::hash(data),
            data: data.to_vec(),
            blocks: Some(blocks.to_vec()),
            edges: edges.to_vec(),
            findings: Vec::new(),
        }
    }

    fn stats(name: &str) -> InstanceStats {
        InstanceStats {
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn shared_corpus_keeps_only_campaign_wide_news() {
        let mut hub = Hub::new("t", Vec::new(), None, None);
        hub.submit(stats("a"), vec![entry(b"1", &[1, 2], &[(1, 2)])], vec![])
            .unwrap();
        // Same coverage from another instance adds nothing; a new edge does.
        hub.submit(
            stats("b"),
            vec![
                entry(b"2", &[1, 2], &[(1, 2)]),
                entry(b"3", &[1, 2], &[(2, 1)]),
            ],
            vec![],
        )
        .unwrap();
        assert_eq!((hub.corpus_len(), hub.redundant, hub.edge_count()), (2, 1, 2));
        // Each instance gets the others' entries, from its cursor on.
        let (to_a, cursor) = hub.entries_since(0, "a", 100);
        assert_eq!((to_a.len(), cursor), (1, 2));
        assert_eq!(to_a[0].data, b"3");
        assert_eq!(hub.entries_since(cursor, "a", 100).0.len(), 0);
        let (to_b, _) = hub.entries_since(0, "b", 1);
        assert_eq!(to_b[0].data, b"1");
    }

    #[test]
    fn findings_are_unique_by_signature() {
        let mut hub = Hub::new("t", Vec::new(), None, None);
        let report = |name: &str| FindingReport {
            signature: "BugCheck:Some(4)".into(),
            finding: Finding {
                class: BugClass::BugCheck,
                detail: name.into(),
                pc: Some(4),
                input: String::new(),
                stack: Vec::new(),
            },
            data: vec![1],
        };
        hub.submit(stats("a"), vec![], vec![report("a")]).unwrap();
        hub.submit(stats("b"), vec![], vec![report("b")]).unwrap();
        let rows: Vec<_> = hub.findings().collect();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].reports, rows[0].instance.as_str()), (2, "a"));
        // An input whose only news is a known finding is redundant.
        let mut known = entry(b"x", &[], &[]);
        known.findings = vec!["BugCheck:Some(4)".into()];
        hub.submit(stats("b"), vec![known], vec![]).unwrap();
        assert_eq!(hub.corpus_len(), 0);
    }

    #[test]
    fn entries_travel_as_hex_json() {
        let original = entry(&[0, 0xff, 7], &[3], &[(3, 4)]);
        let text = serde_json::to_string(&original).unwrap();
        assert!(text.contains("\"00ff07\""));
        let back: Entry = serde_json::from_str(&text).unwrap();
        assert_eq!((back.data, back.blocks, back.edges), (original.data, original.blocks, original.edges));
    }
}
