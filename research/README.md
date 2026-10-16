# kernel debugger

Windows ARM64 · macOS/HVF · experimental. No KDNET or guest driver.
Optional; excluded from normal vroom builds.

## run

```sh
research/run.sh /path/to/lab.toml
research/run.sh /path/to/lab.toml --restore /path/to/checkpoint
research/run.sh /path/to/lab.toml --atui /path/to/run-atui-macos.sh
research/demo.sh CONFIG CHECKPOINT       # paused Ex callback
research/build.sh                        # build + sign
```

Requires `uv`, QEMU firmware, swtpm. Nested virtualization off.
Use a disposable overlay. Separate state directory. Starts paused.

## keys

| Key | Action |
|---|---|
| Enter | Open selected callback/function: assembly + C |
| F2 | Toggle execution breakpoint |
| F3 | Analyze current address |
| F4 | Kernel + symbols + modules |
| F5 / F6 / F7 | Continue / pause / instruction step |
| F9 | Side panels |
| F10 | Expand Code view |
| : | Command |
| Ctrl+Q | Power off + quit |

Code: assembly left, angr C right. Requires `--atui`.
Live/File button: paused memory or matching binary. Cached while paused.
`PARTIAL`: missing pages. `reloc?`: unresolved absolute internal pointers.

## commands

```text
functions ntoskrnl Ps           prefix filter; Enter opens function
next / prev                    function pages (200 rows)
symbols ntdll                   download/cache matching PDB
x nt!KeBugCheckEx               resolve
dis nt!KeBugCheckEx             assembly + C
bp nt!KeBugCheckEx              execution breakpoint; 4 slots
bc                             clear execution breakpoints
watch ADDRESS 8 w              data watchpoint; r/w/rw; 2 slots
wc                             clear watchpoints
ps                             processes; Enter selects context
process 1234                   select PID for memory/symbols
process                        return to CPU memory context
peb                            PEB, image path, command line
modules                        kernel modules
process 1234                   kernel + selected process modules
functions ntdll NtCreate        user functions
bp ntdll!NtCreateFile           process-scoped user breakpoint
cpu 1                          select CPU registers/stepping
stack                          selected CPU stack + symbols
mem ADDRESS 256                virtual memory
image ntoskrnl                  fetch verified full binary
image example.sys /path/file    use matching local binary
dec-file nt!KeGetCurrentIrql    full binary → angr_tui
dec ADDRESS                    live image → angr_tui; strict
dec-partial ADDRESS            allow missing pages; labelled
dump ADDRESS 4096              raw memory file in state/exports
callbacks                      process callback registrations
event                          Ex/Ex2 arguments at entry
checkpoint /new/directory       save paused VM
```

## semantics

- Process context: inspection; no guest injection. CPU registers stay CPU-local.
- User execution breakpoints: selected address space. Kernel breakpoints/watchpoints: all CPUs.
- Watch ranges: 1–8 bytes within one aligned 8-byte block.
- Stack: ARM64 packed/xdata, partial prologs/epilogs. PAC stripped, not authenticated.
- Unknown unwind records, trap/context frames, SVE, ARM64EC: stop with reason.
- PDBs: GUID/age checked. Binaries: ARM64, timestamp, image size, GUID/age checked.
- File analysis: original code/data; no live patches, resolved imports, or heap state.
- Partial live images: absent/discarded pages. Full binaries may exist on Microsoft's server; local files work too.
- Decompilation: angr recovery limits apply; some ARM64 syscall stubs remain assembly-only.
- Guest paging: resident pages only. No page-in injection, WOW64, KDNET, KVM debugging, or WinDbg dump reader.
- Interrupts remain visible during stepping. No step-over/out or register/memory editing yet.

## speed

32 MiB paused-state page cache. Invalidated before execution.
Parsed PDBs cached. Function lookup: `O(log n + page)`.
Unwind tables cached; binary search. Exports and decompilation reused while paused.
Function filtering debounced; 200-row pages. Downloads/angr run outside TUI rendering.

## layout

```text
research/kernel/     backend, PE/PDB, process inspection, unwind
research/tui/        Textual, Capstone, optional angr_tui
experiments/         ignored local scripts/captures
*.state/             ignored symbols, exports, VM state
```

## checks

```sh
cargo test -p vroom-hv -p vroom-vmm -p vroom-kernel -p vroom-cli
uv run --project research/tui pytest research/tui/tests
cargo build -p vroom-kernel --example debug_smoke
scripts/sign.sh target/debug/examples/debug_smoke
target/debug/examples/debug_smoke
```
