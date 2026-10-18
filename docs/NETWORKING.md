# Networking

## NAT

Unprivileged IPv4 NAT, one instance per adapter:

| Adapter | Network | Guest | Gateway | DNS |
|---|---|---|---|---|
| 1st | `10.0.2.0/24` | `.15` | `.2` | `.3` |
| 2nd | `10.0.3.0/24` | `.15` | `.2` | `.3` |
| nth | `10.0.(n+1).0/24` | | | |

DHCP gives address, route, DNS and a 1500-byte MTU. Change the addresses
with `nat_guest`, `nat_gateway`, `nat_dns`, `nat_gateway_mac` in
[`[identity]`](CONFIG.md#identity).

smoltcp faces the guest; ordinary host sockets go out. It is a proxy, not
a router: host routing and VPNs decide the path.

Supported:

- TCP (half-close, backpressure), TCP port forwards.
- UDP (DNS, QUIC); DNS over TCP through the same relay.
- ICMP echo through unprivileged datagram ping sockets (no raw sockets).
  Linux hosts: allowed by `net.ipv4.ping_group_range`; vroom doesn't
  change it.
- IPv4 fragment reassembly (out of order, overlaps rejected, capped) and
  fragmentation of large replies.
- ICMP time-exceeded for expired TTLs; destination errors for refused
  echoes.

Policy: loopback, link-local, multicast and the virtual subnets are
blocked. `allow_host_loopback` opens the gateway as an alias for the
host's loopback (TCP/UDP). `block_private` also blocks LAN and CGNAT. The
DNS alias forwards to the host's first IPv4 nameserver in
`/etc/resolv.conf`.

## Changing adapters while the VM runs

From the bar's network button or the settings page; both write the VM
file.

- **Cable out** (`connected = false`): link down (`NO-CARRIER`, "Network
  cable unplugged"), no frames. Back in: the guest re-DHCPs.
- **Another network**: same device and MAC, new backend. Link bounces for
  2 s, the guest re-DHCPs, old connections end.
- vmnet only passes the MAC it picked: the VMM rewrites MACs both ways
  (Ethernet, ARP, DHCP, IPv6 ND). The guest keeps its MAC.
- NAT live needs a VM that started with a NAT adapter (the sandbox only
  allows outbound sockets then).
- Port forwards need listening sockets the sandbox can't open: an adapter
  keeps its start-time NAT and forwards while on another network, and
  gets them back on return. New forwards wait for the next start.
- **New adapter**: on PCI VMs (UEFI, or `virtio_transport = "pci"`) it
  goes into one of four PCIe hot plug slots; the guest has it in seconds.
  On virtio-mmio: next start.
- **Removing a hot-plugged adapter**: attention button, the guest lets go
  (15 s at most, then forced). An adapter the VM started with is
  unplugged now, gone at the next start.
- A new MAC waits for the next start.
- Next start: all adapters are ordinary root-bus devices again.
- Resume: the saved adapters, in their slots, set up as the file says
  now. Adapters added while suspended wait for the next start.

The sandboxed VMM can't reach vroom-netd: the app gets the port from netd
and hands the socket over the viewer connection
([SECURITY.md](SECURITY.md#network-changes-while-the-vm-runs)).

## Bridged and host-only

Through `vroom-netd` (root): vmnet on macOS, tap + bridge on Linux. The
app sets it up; by hand: [GUIDE.md](GUIDE.md#the-root-helper-vroom-netd).

- Bridged tested on a Mac's Wi-Fi (`en0`): LAN address, internet, Mac ↔
  guest.
- netd accepts only locally administered unicast MACs (`x2:`, `x6:`,
  `xA:`, `xE:`; the default `02:...` is one), refuses other source MACs,
  and caps 64 connections and 32 ports per user.
- Host-only on macOS: a vmnet network by name, no DHCP and no host
  address on it. Use static addresses.

## Limits

- No IPv6, RA, DHCPv6, NAT64.
- Port forwards: TCP only.
- ICMP: echo relay only. No remote ICMP errors, traceroute or guest path
  MTU discovery.
- DNS doesn't follow macOS scoped/split resolvers or host network changes.
- Fragments: 64 datagrams, 128 pieces each, 30 s. Echo probes: 64, 10 s.
- TCP connects time out at 15 s; UDP mappings idle out at 60 s.
- NAT connections don't survive a snapshot restore.
- virtio-net: one queue pair, no checksum/segmentation offloads.

## Tests

```sh
cargo test -p vroom-net                           # packets and localhost sockets
cargo test -p vroom-net -- --ignored --nocapture  # needs internet: sandboxed echo to 1.1.1.1
```

Guest smoke test (NAT): DHCP, DNS, HTTP, `ping -c 3 1.1.1.1`,
`ping -c 2 -s 4001 1.1.1.1` (fragmented).
