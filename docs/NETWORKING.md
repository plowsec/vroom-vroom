# Networking

## NAT

Unprivileged IPv4 NAT, one instance per adapter:

| Adapter | Network | Guest | Gateway | DNS |
|---|---|---|---|---|
| 1st | `10.0.2.0/24` | `.15` | `.2` | `.3` |
| 2nd | `10.0.3.0/24` | `.15` | `.2` | `.3` |
| nth | `10.0.(n+1).0/24` | | | |

- DHCP: address, route, DNS, 1500-byte MTU.
- Other addresses: `nat_guest`, `nat_gateway`, `nat_dns`, `nat_gateway_mac` in [`[identity]`](CONFIG.md#identity).
- smoltcp faces the guest; plain host sockets go out. A proxy, not a router: host routing and VPNs pick the path.

Supported:

- TCP (half-close, backpressure), TCP port forwards.
- UDP (DNS, QUIC); DNS over TCP through the same relay.
- ICMP echo via unprivileged datagram ping sockets (no raw sockets). Linux hosts: needs `net.ipv4.ping_group_range`; vroom doesn't change it.
- IPv4 fragment reassembly (out of order, overlaps rejected, capped); fragmentation of large replies.
- ICMP time-exceeded for expired TTLs; destination errors for refused echoes.

Policy:

- Blocked: loopback, link-local, multicast, the virtual subnets, the host's own addresses (LAN, VPN, bridge; checked per new flow, TCP/UDP/ping).
- `allow_host_loopback`: gateway = alias for host loopback (TCP/UDP); host's own addresses open too.
- `host_ports`: gateway alias only.
- `block_private`: also LAN and CGNAT.
- DNS alias forwards to the host's first IPv4 nameserver in `/etc/resolv.conf`.

`mirror`: guest IPv4 frames to that network come straight back, MACs and addresses swapped (checksums unchanged), fragments one by one. A guest client reaches the guest's own services as a remote peer, through the NIC's receive path.

## Changing adapters while the VM runs

Bar's network button or settings page; both write the VM file.

| Change | Effect |
|---|---|
| cable out (`connected = false`) | link down (`NO-CARRIER`, "Network cable unplugged"), no frames; back in: guest re-DHCPs |
| another network | same device and MAC, new backend; link bounces 2 s, re-DHCP, old connections end |
| new adapter | PCI VMs (UEFI, or `virtio_transport = "pci"`): one of 4 PCIe hot plug slots, guest has it in seconds. virtio-mmio: next start |
| remove hot-plugged adapter | attention button, guest lets go (≤15 s, then forced) |
| remove start-time adapter | unplugged now, gone next start |
| new MAC, new port forwards | next start |

- vmnet passes only the MAC it picked: the VMM rewrites MACs both ways (Ethernet, ARP, DHCP, IPv6 ND). Guest keeps its MAC.
- NAT live: only if the VM started with a NAT adapter (sandbox allows outbound sockets only then).
- Port forwards need listening sockets the sandbox can't open: an adapter keeps its start-time NAT and forwards while on another network, gets them back on return.
- Next start: all adapters plain root-bus devices again.
- Resume: saved adapters in their slots, set up as the file says now. Adapters added while suspended: next start.
- Sandboxed VMM can't reach vroom-netd: the app gets the port from netd, hands the socket over the viewer connection ([SECURITY.md](SECURITY.md#network-changes-while-the-vm-runs)).

## Bridged and host-only

Via `vroom-netd` (root): vmnet on macOS, tap + bridge on Linux. App sets it up; by hand: [GUIDE.md](GUIDE.md#the-root-helper-vroom-netd).

- Bridged tested on Mac Wi-Fi (`en0`): LAN address, internet, Mac ↔ guest.
- netd: only locally administered unicast MACs (`x2:`, `x6:`, `xA:`, `xE:`; default `02:...` is one); other source MACs refused; 64 connections, 32 ports per user.
- Host-only: private network of host and VMs, no way out.

## Host-only

```toml
[[net]]
mode = "host-only"     # network = "lab", the default
```

| Network | Host | Guests (DHCP) |
|---|---|---|
| `lab` (default) | `192.168.77.1` | `.2` to `.254`, the same one per VM adapter |

```bash
vroom net                         # each running VM's address
ssh user@192.168.77.2
```

- macOS: vmnet host mode. vmnet makes a `bridgeN` with the host's address, serves DHCP (`bootpd`, leases in `/var/db/dhcpd_leases`). Host doesn't route or NAT for it.
- Linux: `bwh-<name>` bridge with the host's address; netd answers guest DHCP itself (no router, no DNS), keeps it off the bridge. Host firewall still decides what reaches the host.
- Lease follows the adapter's MAC (macOS: vmnet's MAC, fixed per VM name and adapter number). Linux: address from a hash of the MAC, stable across netd restarts; full pool reuses the address of a MAC silent for 1 h.
- VMs on one network reach each other; other networks = other subnets, other bridges.
- More networks: `/etc/vroom/netd.toml`; private ranges, /16 to /29, no overlaps; host = the address given, DHCP the rest above it:

  ```toml
  [[host_only]]
  name = "lab"
  gateway = "192.168.77.1/24"
  [[host_only]]
  name = "isolated"
  gateway = "10.77.0.1/24"
  ```

  No `[[host_only]]`: `lab` as above. Restart: `sudo launchctl kickstart -k system/org.vroom.netd` (Linux: `sudo systemctl restart vroom-netd`).
- Windows: unidentified public network, firewall drops ping and most inbound. `Set-NetConnectionProfile -NetworkCategory Private`, or allow the rule (OpenSSH Server adds its own for 22).
- macOS firewall with "Automatically allow built-in software" off: unblock `/usr/libexec/bootpd`, or no DHCP.
- Old netd (`vroom net` says so): no DHCP on host-only (macOS: isolated vmnet network, host not on it). Set it up again (app asks at VM start; Linux: reinstall).

## Limits

- No IPv6, RA, DHCPv6, NAT64.
- Port forwards: TCP only.
- ICMP: echo relay only. No remote ICMP errors, traceroute, guest path MTU discovery.
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

Guest smoke test (NAT): DHCP, DNS, HTTP, `ping -c 3 1.1.1.1`, `ping -c 2 -s 4001 1.1.1.1` (fragmented).
