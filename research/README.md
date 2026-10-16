# kernel debugger

Windows ARM64 · macOS/HVF · experimental. No KDNET or guest driver.
Optional: normal vroom builds exclude this tool and its dependencies.

## run

```sh
research/run.sh /path/to/lab.toml
research/run.sh /path/to/lab.toml --restore /path/to/checkpoint
research/run.sh /path/to/lab.toml --atui /path/to/run-atui-macos.sh
research/demo.sh CONFIG CHECKPOINT      # paused Ex callback
research/build.sh                       # build + sign only
```

Requires `uv`, QEMU firmware, swtpm. Nested virtualization off.
Starts paused. F5 boots; F6 pauses; F4 loads Windows symbols.
Ctrl+Q powers off. Save a checkpoint first.

## keys

| Key | Action |
|---|---|
| F2 | Toggle breakpoint |
| F4 | Kernel + symbols + modules |
| F5 / F6 / F7 | Continue / pause / step |
| F9 | Panels (narrow terminal) |
| : | Command |
| Ctrl+Q | Power off + quit |

## commands

```text
x nt!KeBugCheckEx               resolve symbol
dis nt!KeBugCheckEx             disassemble
bp nt!KeBugCheckEx              breakpoint (4 slots, all CPUs)
bc                             clear breakpoints
cpu 1                          select CPU
modules                        loaded kernel modules
callbacks                      process callback registrations
event                          Ex/Ex2 arguments at callback entry
mem 0xffff... 256               virtual memory
dec 0xffff...                   angr_tui; complete image required
dec-partial 0xffff...           allow missing pages; marked incomplete
checkpoint /new/directory       save paused VM
```

## status

| Capability | State |
|---|---|
| Kernel PDB download/cache | GUID + age checked |
| Named kernel symbols | Resolve, disassemble, break |
| Modules / process callbacks | Live inspection |
| Registers / memory / stepping | HVF; 4 KiB page tables |
| angr_tui | RVA view; partial exports labelled |
| Function search/browser | Missing |
| Process selection / userland attach | Missing |
| PEB / user modules / user symbols | Missing |
| Stack unwind / watchpoints / dumps / KVM | Missing |

Pending interrupts remain visible during stepping.
Callback layouts: PDB types or recognized kernel instructions.
Decompiler types are inferred. Missing pages/pointers limit analysis.

## disks

Use a disposable overlay. Base and overlay in the same directory.

```sh
qemu-img create -f qcow2 -F qcow2 -b /absolute/base.qcow2 lab.qcow2
# Point lab.toml at lab.qcow2. Separate state directory. No shares/network.
python3 research/kernel/tools/stage_vm.py SOURCE.vmwarevm NEW.vmwarevm
# Add --copy to stage a stopped VM. Source untouched; hashes checked.
```

## layout

```text
research/kernel/     backend, PE/PDB, page tables
research/tui/        Textual, Capstone, optional angr_tui
experiments/         ignored local work
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
