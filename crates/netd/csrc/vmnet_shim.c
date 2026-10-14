// Synchronous wrapper around vmnet.framework (which is block/dispatch
// based). One bw_vmnet per VM port. Needs root (or Apple's restricted
// com.apple.vm.networking entitlement).
#include <vmnet/vmnet.h>
#include <dispatch/dispatch.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    interface_ref iface;
    dispatch_queue_t queue;
    dispatch_semaphore_t avail;
    uint64_t max_packet;
    uint8_t mac[6];
} bw_vmnet;

// mode: 0 = host-only, 1 = bridged. netid may be NULL (default host net).
bw_vmnet *bw_vmnet_start(int mode, const char *bridge_if, const unsigned char *iface_uuid,
                         const unsigned char *netid, char *err, size_t errlen) {
    xpc_object_t desc = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, mode ? VMNET_BRIDGED_MODE : VMNET_HOST_MODE);
    if (mode && bridge_if)
        xpc_dictionary_set_string(desc, vmnet_shared_interface_name_key, bridge_if);
    xpc_dictionary_set_uuid(desc, vmnet_interface_id_key, iface_uuid);
    if (!mode && netid) {
        if (__builtin_available(macOS 11.0, *)) {
            xpc_dictionary_set_uuid(desc, vmnet_network_identifier_key, netid);
        }
    }
    bw_vmnet *v = calloc(1, sizeof(*v));
    v->queue = dispatch_queue_create("org.bulwark.vmnet", DISPATCH_QUEUE_SERIAL);
    v->avail = dispatch_semaphore_create(0);
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    __block vmnet_return_t st = VMNET_FAILURE;
    bw_vmnet *vv = v;
    v->iface = vmnet_start_interface(desc, v->queue, ^(vmnet_return_t status, xpc_object_t params) {
        st = status;
        if (status == VMNET_SUCCESS) {
            vv->max_packet = xpc_dictionary_get_uint64(params, vmnet_max_packet_size_key);
            const char *m = xpc_dictionary_get_string(params, vmnet_mac_address_key);
            unsigned int b[6] = {0};
            if (m && sscanf(m, "%x:%x:%x:%x:%x:%x", &b[0], &b[1], &b[2], &b[3], &b[4], &b[5]) == 6)
                for (int i = 0; i < 6; i++) vv->mac[i] = (uint8_t)b[i];
        }
        dispatch_semaphore_signal(done);
    });
    xpc_release(desc);
    if (!v->iface) {
        snprintf(err, errlen, "vmnet_start_interface returned NULL (root needed?)");
        free(v);
        return NULL;
    }
    dispatch_semaphore_wait(done, DISPATCH_TIME_FOREVER);
    if (st != VMNET_SUCCESS) {
        snprintf(err, errlen, "vmnet start failed: status %d", (int)st);
        free(v);
        return NULL;
    }
    dispatch_semaphore_t avail = v->avail;
    vmnet_interface_set_event_callback(v->iface, VMNET_INTERFACE_PACKETS_AVAILABLE, v->queue,
        ^(interface_event_t ev, xpc_object_t e) { (void)ev; (void)e; dispatch_semaphore_signal(avail); });
    return v;
}

void bw_vmnet_info(bw_vmnet *v, uint8_t *mac, uint64_t *max_packet) {
    memcpy(mac, v->mac, 6);
    *max_packet = v->max_packet;
}

// Returns bytes read, 0 on timeout, -1 on error.
long bw_vmnet_read(bw_vmnet *v, uint8_t *buf, size_t len, int timeout_ms) {
    for (int tries = 0; tries < 2; tries++) {
        struct iovec iov = { buf, len };
        struct vmpktdesc pd = { .vm_pkt_size = len, .vm_pkt_iov = &iov, .vm_pkt_iovcnt = 1, .vm_flags = 0 };
        int cnt = 1;
        if (vmnet_read(v->iface, &pd, &cnt) != VMNET_SUCCESS) return -1;
        if (cnt == 1) return (long)pd.vm_pkt_size;
        if (tries == 0 &&
            dispatch_semaphore_wait(v->avail, dispatch_time(DISPATCH_TIME_NOW, (int64_t)timeout_ms * 1000000)) != 0)
            return 0;
    }
    return 0;
}

int bw_vmnet_write(bw_vmnet *v, const uint8_t *buf, size_t len) {
    struct iovec iov = { (void *)buf, len };
    struct vmpktdesc pd = { .vm_pkt_size = len, .vm_pkt_iov = &iov, .vm_pkt_iovcnt = 1, .vm_flags = 0 };
    int cnt = 1;
    return vmnet_write(v->iface, &pd, &cnt) == VMNET_SUCCESS && cnt == 1 ? 0 : -1;
}

void bw_vmnet_stop(bw_vmnet *v) {
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    vmnet_stop_interface(v->iface, v->queue, ^(vmnet_return_t s) { (void)s; dispatch_semaphore_signal(done); });
    dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 5LL * 1000000000));
    free(v);
}
