# vroom fuzz lab

This package is optional research code. It is not a dependency of the normal
vroom binaries.

The input manifest is deliberately explicit:

```json
{
  "name": "driver-dispatch",
  "config": "vm.toml",
  "checkpoint": "checkpoint.state",
  "state_dir": "state",
  "input_gpa": 1048576,
  "input_capacity": 4096,
  "coverage": [
    {"id": 1, "gpa": 2097152, "va": null, "size": 4,
     "module": "example.sys", "function": "dispatch", "source": null}
  ],
  "detectors": ["access-violation", "bugcheck", "pool-corruption"]
}
```

`coverage` is the audit surface. Keep the disassembler or symbol input that
produced it beside the manifest. The runner writes `coverage.json` and
`blocks.json`; the TUI turns those records into per-function progress rows.
Corpus entries are content addressed, and findings retain their class, input
hash and stop detail.

Building this package enables the VMM research feature through its dependency.
Normal `cargo build` and the shipped vroom binaries do not compile this
package or the research VMM API.
