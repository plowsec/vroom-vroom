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

These interfaces prepare for another board. The x64 VMM builder remains gated;
this change adds no UART receive worker, guest agent transport, new board,
snapshot support, or hardware-runtime acceptance.
