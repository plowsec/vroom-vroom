// CHPE metadata for the EC view of an ARM64X image (what the MSVC CRT's
// chpe object provides). The linker fills the symbols it names; the
// __os_arm64x_* slots are filled by the OS loader.
        .section .rdata,"dr"
        .globl __chpe_metadata
        .p2align 3, 0
__chpe_metadata:
        .word 2
        .rva __hybrid_code_map
        .word __hybrid_code_map_count
        .rva __x64_code_ranges_to_entry_points
        .rva __arm64x_redirection_metadata
        .rva __os_arm64x_dispatch_call_no_redirect
        .rva __os_arm64x_dispatch_ret
        .rva __os_arm64x_check_call
        .rva __os_arm64x_check_icall
        .rva __os_arm64x_check_icall_cfg
        .rva __arm64x_native_entrypoint
        .rva __hybrid_auxiliary_iat
        .word __x64_code_ranges_to_entry_points_count
        .word __arm64x_redirection_metadata_count
        .rva __os_arm64x_get_x64_information
        .rva __os_arm64x_set_x64_information
        .rva __arm64x_extra_rfe_table
        .word __arm64x_extra_rfe_table_size
        .rva __os_arm64x_dispatch_fptr
        .rva __hybrid_auxiliary_iat_copy
        .rva __hybrid_auxiliary_delayload_iat
        .rva __hybrid_auxiliary_delayload_iat_copy
        .word __hybrid_image_info_bitfield
        .rva __os_arm64x_helper3
        .rva __os_arm64x_helper4
        .rva __os_arm64x_helper5
        .rva __os_arm64x_helper6
        .rva __os_arm64x_helper7
        .rva __os_arm64x_helper8

        .data
        .globl __os_arm64x_dispatch_call_no_redirect
__os_arm64x_dispatch_call_no_redirect: .xword 0
        .globl __os_arm64x_dispatch_ret
__os_arm64x_dispatch_ret: .xword 0
        .globl __os_arm64x_check_call
__os_arm64x_check_call: .xword 0
        .globl __os_arm64x_check_icall
__os_arm64x_check_icall: .xword 0
        .globl __os_arm64x_check_icall_cfg
__os_arm64x_check_icall_cfg: .xword 0
        .globl __os_arm64x_get_x64_information
__os_arm64x_get_x64_information: .xword 0
        .globl __os_arm64x_set_x64_information
__os_arm64x_set_x64_information: .xword 0
        .globl __os_arm64x_dispatch_fptr
__os_arm64x_dispatch_fptr: .xword 0
        .globl __os_arm64x_helper3
__os_arm64x_helper3: .xword 0
        .globl __os_arm64x_helper4
__os_arm64x_helper4: .xword 0
        .globl __os_arm64x_helper5
__os_arm64x_helper5: .xword 0
        .globl __os_arm64x_helper6
__os_arm64x_helper6: .xword 0
        .globl __os_arm64x_helper7
__os_arm64x_helper7: .xword 0
        .globl __os_arm64x_helper8
__os_arm64x_helper8: .xword 0
