// Shims for HVF calls that pass clang vector types by value.
// Rust can't express that ABI safely, so we go through a pointer.
#include <Hypervisor/Hypervisor.h>
#include <string.h>

hv_return_t bw_hv_vcpu_set_simd_fp_reg(hv_vcpu_t vcpu, hv_simd_fp_reg_t reg, const uint8_t *bytes) {
    hv_simd_fp_uchar16_t v;
    memcpy(&v, bytes, sizeof(v));
    return hv_vcpu_set_simd_fp_reg(vcpu, reg, v);
}

hv_return_t bw_hv_vcpu_get_simd_fp_reg(hv_vcpu_t vcpu, hv_simd_fp_reg_t reg, uint8_t *bytes) {
    hv_simd_fp_uchar16_t v;
    hv_return_t r = hv_vcpu_get_simd_fp_reg(vcpu, reg, &v);
    memcpy(bytes, &v, sizeof(v));
    return r;
}
