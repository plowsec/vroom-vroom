# Board capability boundaries

`Vmm::serial_input` now returns `Result<()>`. The CLI reports an input rejection
and continues running the VM. The ARM board still queues bytes in its existing
PL011 receive FIFO and uses the same output sink and interrupt wiring. A board
without a serviced receive path must select `SerialInput::Unavailable`.

`Agent::unavailable` represents a board without an agent transport. It creates
no socket or service thread. This differs from the existing service waiting for
the guest to connect; ARM keeps that service and its behavior unchanged.
Unavailable agents report a disconnected status and zero features. Exec, file
transfer and update requests receive the explicit reason immediately. Direct
reply-bearing `AgentCmd` requests receive the same error; fire-and-forget
commands remain best effort and are dropped when unavailable.

The control socket decodes the first request frame and returns its matching
failure frame before reading a transfer body, cloning a socket, or creating a
transfer pipe or forwarding thread. The regular listener/request handler may
still run. There is no claim that a guest operation succeeded.

The strict x64 direct-Linux reference board selects both unavailable states.
Its serial output is separate from input. It adds no receive worker or guest
agent transport. See `X64_LINUX.md` for its bounded boot/control acceptance and
limitations; capability tests alone establish no hardware-runtime acceptance.
