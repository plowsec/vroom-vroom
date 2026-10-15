# Networking

Vroom currently provides an unprivileged IPv4 NAT backend. The guest sees
`10.0.2.15/24`, gateway `10.0.2.2`, and DNS relay `10.0.2.3`. DHCP supplies the
address, route, DNS server and 1500-byte MTU. Each adapter has its own NAT
instance on its own network: the second NAT adapter is `10.0.3.0/24`
(guest `.15`, gateway `.2`, DNS `.3`), the third `10.0.4.0/24`, and so on.

The backend uses smoltcp for guest-facing TCP and normal host sockets for
outbound traffic. It is a proxy, not a transparent IP router: host routing and
VPN settings determine the actual path to the internet.

## Supported

- TCP connections, half-close, backpressure, and explicit TCP port forwards.
- UDP flows, including DNS and QUIC traffic. DNS over TCP uses the same relay.
- External ICMP echo requests through unprivileged datagram ping sockets.
  Replies preserve the guest's identifier, sequence number and payload.
- IPv4 fragment reassembly and fragmentation of large UDP/echo replies at the
  guest MTU. Reassembly accepts out-of-order fragments, rejects overlaps, and
  caps both memory use and lifetime.
- ICMP time-exceeded at the virtual gateway for expired TTLs, and destination
  errors for rejected echo destinations or unavailable ping sockets.

The default policy blocks direct loopback, link-local, multicast and virtual
subnet destinations. `allow_host_loopback` enables the gateway TCP/UDP alias;
`block_private` additionally blocks LAN and CGNAT destinations. The DNS alias
uses the host's first IPv4 nameserver from `/etc/resolv.conf`, even when that
resolver is private. Echo forwarding does not use either alias.

No raw sockets or new sandbox permissions are needed for echo forwarding. On
Linux, the account must be allowed to create datagram ping sockets by the host's
`net.ipv4.ping_group_range` policy. Vroom does not change this setting or fall
back to privileged raw sockets. Darwin includes an IPv4 header in ping replies;
Linux supplies the ICMP payload and rewrites the echo identifier. The relay
handles both layouts and matches outstanding requests before returning replies.
See the [Linux ICMP documentation](https://man7.org/linux/man-pages/man7/icmp.7.html)
and [Darwin ICMP implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/netinet/ip_icmp.c).

## Limits and unfinished work

- IPv6, router advertisements, DHCPv6 and NAT64 are not implemented.
- Bridged and host-only go through `vroom-netd` (root): vmnet on macOS,
  tap + bridge on Linux. The plumbing is tested, but not yet against a real
  vmnet/tap (needs root). netd only accepts locally administered unicast
  MACs (`x2:`, `x6:`, `xA:`, `xE:`...), so a VM can't take a real device's
  address on the LAN; the default per-VM MACs (`02:...`) are such. It
  refuses frames from any other source MAC, and serves at most 64
  connections at once and 32 ports per user.
- Port forwarding supports TCP only. UDP inbound forwarding is not implemented.
- This is an echo relay, not general ICMP forwarding. Remote ICMP errors,
  end-to-end traceroute, and guest-visible path MTU discovery remain incomplete.
  TCP uses the host stack's path MTU handling; UDP socket failures still do not
  produce complete guest ICMP diagnostics.
- DNS discovery does not yet follow macOS scoped/split resolvers or refresh
  dynamically after a host network change.
- Fragments are limited to 64 simultaneous datagrams, 128 pieces per datagram,
  and 30 seconds of lifetime. Overlaps, including duplicates, discard the
  pending datagram. Echo probes are capped at 64 and expire after 10 seconds.
- TCP connects expire after 15 seconds; UDP mappings idle out after 60 seconds.
  NAT connections cannot survive a VM snapshot restore.
- virtio-net uses one queue pair without checksum/segmentation offloads.

## Verification

Run deterministic packet and localhost socket tests:

```sh
cargo test -p vroom-net
```

The integration tests exercise an 8 KB fragmented UDP round trip, a 300 KB TCP
exchange with half-close, malformed/overlapping fragments, policy rejection,
TTL handling, and connection/probe expiry.

Opt-in tests require internet access:

```sh
cargo test -p vroom-net -- --ignored --nocapture
```

The external echo test launches a separate process, applies the production
sandbox, and sends a fragmented 4 KB probe to `1.1.1.1`. A network that filters
ICMP or denies datagram ping sockets can fail that test even when TCP works.

For a guest smoke test, boot a disposable Ubuntu overlay with `[[net]] mode =
"nat"`, then check DHCP, DNS and HTTP alongside `ping -c 3 1.1.1.1` and
`ping -c 2 -s 4001 1.1.1.1`. A successful HTTP request alone does not establish
that ICMP, large datagrams, or application-specific requests work.
